//! Headless ACP (`Agent Client Protocol`) runner implementing [`crate::runner::Runner`]
//! (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
//!
//! Invokes **only** an official, unmodified ACP-compliant vendor binary configured via
//! `[acp_runner]` in `config.toml` (or `BOARD_ACP_BIN`) over standard newline-delimited
//! JSON-RPC 2.0 stdio:
//! - `initialize` -> `session/new` -> `session/prompt` on [`Runner::spawn`],
//! - live `session/prompt` (with queueing while busy) or respawn + `session/load` (recording a
//!   Trail Rule entry before falling back to `session/new` when unsupported) on [`Runner::send`],
//! - `session/cancel` + process-group `SIGTERM` -> `SIGKILL` teardown on [`Runner::cancel`] and
//!   turn timeouts, always writing a Trail Rule record to `$BOARD_HOME/state/runner-cancels.jsonl`
//!   before acting,
//! - optional Linux `systemd-run --user --scope` cgroup isolation (`MemoryMax`, `CPUQuota`,
//!   `TasksMax`, `Slice`),
//! - durable `session_id` + `pid` persistence in `$BOARD_HOME/state/daemon.db` (`local_runs`)
//!   and raw JSONL transcript logging in `$BOARD_HOME/state/transcripts/<run-id>.jsonl`.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{BoardSlug, Runtime, Timestamp};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::config::{AcpPermissionPolicy, AcpRunnerConfig, CgroupConfig, Home, ThreadModel};
use crate::local_db::{LocalDb, LocalRunRecord, LocalRunState};
use crate::runner::{
    Clock, RunEvent, RunEventKind, RunHandle, RunSpec, Runner, RunnerState, RunnerStatus,
    SteerMessage, clip_event_summary, extract_wake_digest, record_cancel_trail, system_clock,
};
use crate::state;

/// Default handshake timeout for `initialize` and `session/new` / `session/load`.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum normalized [`RunEvent`]s kept in memory per session (full raw traffic is always
/// appended to `$BOARD_HOME/state/transcripts/<run-id>.jsonl`).
const MAX_IN_MEMORY_EVENTS: usize = 10_000;

/// Maximum recent stderr lines kept for error diagnostics.
const MAX_STDERR_TAIL: usize = 16;

/// Resolved program and arguments used to spawn an official ACP binary (directly or wrapped
/// inside `systemd-run --user --scope` on Linux when cgroup isolation is enabled).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
}

/// Builds the exact command line for launching `bin` with `args`, optionally wrapping it in
/// `systemd-run --user --scope --quiet` with the configured cgroup properties when
/// `use_systemd && cgroup.enabled`.
pub fn build_launch_command(
    bin: &Path,
    args: &[String],
    cgroup: &CgroupConfig,
    use_systemd: bool,
) -> LaunchSpec {
    if use_systemd && cgroup.enabled {
        let mut wrapped = vec![
            OsString::from("--user"),
            OsString::from("--scope"),
            OsString::from("--quiet"),
        ];
        if let Some(memory_max) = &cgroup.memory_max {
            wrapped.push(OsString::from("-p"));
            wrapped.push(OsString::from(format!("MemoryMax={memory_max}")));
        }
        if let Some(cpu_quota) = &cgroup.cpu_quota {
            wrapped.push(OsString::from("-p"));
            wrapped.push(OsString::from(format!("CPUQuota={cpu_quota}")));
        }
        if let Some(tasks_max) = cgroup.tasks_max {
            wrapped.push(OsString::from("-p"));
            wrapped.push(OsString::from(format!("TasksMax={tasks_max}")));
        }
        if let Some(slice) = &cgroup.slice {
            wrapped.push(OsString::from("-p"));
            wrapped.push(OsString::from(format!("Slice={slice}")));
        }
        wrapped.push(OsString::from("--"));
        wrapped.push(bin.as_os_str().to_os_string());
        wrapped.extend(args.iter().map(OsString::from));
        LaunchSpec {
            program: OsString::from("systemd-run"),
            args: wrapped,
        }
    } else {
        LaunchSpec {
            program: bin.as_os_str().to_os_string(),
            args: args.iter().map(OsString::from).collect(),
        }
    }
}

/// Headless ACP [`Runner`] speaking standard ACP JSON-RPC 2.0 over stdio to an official,
/// unmodified vendor binary.
#[derive(Clone)]
pub struct AcpRunner {
    inner: Arc<AcpRunnerInner>,
}

struct AcpRunnerInner {
    config: AcpRunnerConfig,
    home: Home,
    local_db: LocalDb,
    clock: Clock,
    turn_timeout: Duration,
    cancel_grace: Duration,
    handshake_timeout: Duration,
    /// Map from handle `native_id` (and any replacement `session_id` after `session/load`
    /// fallback) to its shared session slot. Never referenced by `run_supervisor`, preventing
    /// `Arc` reference cycles so [`Drop`] always runs when the last [`AcpRunner`] is dropped.
    sessions: Mutex<BTreeMap<String, Arc<SessionSlot>>>,
}

/// Detached context owned by each background [`run_supervisor`] task (holding no reference to
/// [`AcpRunnerInner`] so dropping the last [`AcpRunner`] tears down live child processes).
#[derive(Clone)]
struct SupervisorContext {
    home: Home,
    local_db: LocalDb,
    clock: Clock,
    turn_timeout: Duration,
    cancel_grace: Duration,
    permission_policy: AcpPermissionPolicy,
}

impl Drop for AcpRunnerInner {
    fn drop(&mut self) {
        let slots: Vec<Arc<SessionSlot>> = {
            let guard = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            guard.values().cloned().collect()
        };
        for slot in slots {
            let pgid = {
                let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.pgid
            };
            #[cfg(unix)]
            if let Some(pg) = pgid {
                let _ = signal_process_group(pg, rustix::process::Signal::KILL);
            }
            if let Ok(mut control) = slot.control.try_lock() {
                control.cmd_tx = None;
                if let Some(task) = control.supervisor.take() {
                    task.abort();
                }
            }
        }
    }
}

impl AcpRunner {
    /// Creates a new headless [`AcpRunner`] backed by `config`, `$BOARD_HOME` (`home`), and
    /// `local_db` (`daemon.db`).
    pub fn new(config: AcpRunnerConfig, home: Home, local_db: LocalDb) -> Self {
        let turn_timeout = Duration::from_secs(config.turn_timeout_secs.max(1));
        let cancel_grace = Duration::from_secs(config.cancel_grace_secs);
        Self {
            inner: Arc::new(AcpRunnerInner {
                config,
                home,
                local_db,
                clock: system_clock(),
                turn_timeout,
                cancel_grace,
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT.min(turn_timeout),
                sessions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Replaces the clock used for [`RunEvent`] timestamps, `LocalDb` updates, transcripts,
    /// and Trail Rule records.
    #[must_use]
    pub fn with_clock(self, clock: Clock) -> Self {
        Self {
            inner: Arc::new(AcpRunnerInner {
                config: self.inner.config.clone(),
                home: self.inner.home.clone(),
                local_db: self.inner.local_db.clone(),
                clock,
                turn_timeout: self.inner.turn_timeout,
                cancel_grace: self.inner.cancel_grace,
                handshake_timeout: self.inner.handshake_timeout,
                sessions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Overrides the per-turn timeout (primarily for sub-second integration tests).
    #[must_use]
    pub fn with_turn_timeout(self, timeout: Duration) -> Self {
        let timeout = timeout.max(Duration::from_millis(10));
        Self {
            inner: Arc::new(AcpRunnerInner {
                config: self.inner.config.clone(),
                home: self.inner.home.clone(),
                local_db: self.inner.local_db.clone(),
                clock: Arc::clone(&self.inner.clock),
                turn_timeout: timeout,
                cancel_grace: self.inner.cancel_grace,
                handshake_timeout: self.inner.handshake_timeout,
                sessions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Overrides the graceful cancel wait before `SIGTERM` / `SIGKILL` escalation (primarily
    /// for sub-second integration tests).
    #[must_use]
    pub fn with_cancel_grace(self, grace: Duration) -> Self {
        Self {
            inner: Arc::new(AcpRunnerInner {
                config: self.inner.config.clone(),
                home: self.inner.home.clone(),
                local_db: self.inner.local_db.clone(),
                clock: Arc::clone(&self.inner.clock),
                turn_timeout: self.inner.turn_timeout,
                cancel_grace: grace,
                handshake_timeout: self.inner.handshake_timeout,
                sessions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Overrides the handshake timeout for `initialize` and `session/new` / `session/load`.
    #[must_use]
    pub fn with_handshake_timeout(self, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(AcpRunnerInner {
                config: self.inner.config.clone(),
                home: self.inner.home.clone(),
                local_db: self.inner.local_db.clone(),
                clock: Arc::clone(&self.inner.clock),
                turn_timeout: self.inner.turn_timeout,
                cancel_grace: self.inner.cancel_grace,
                handshake_timeout: timeout.max(Duration::from_millis(50)),
                sessions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Returns the configured [`AcpRunnerConfig`].
    pub fn config(&self) -> &AcpRunnerConfig {
        &self.inner.config
    }

    fn supervisor_ctx(&self) -> SupervisorContext {
        SupervisorContext {
            home: self.inner.home.clone(),
            local_db: self.inner.local_db.clone(),
            clock: Arc::clone(&self.inner.clock),
            turn_timeout: self.inner.turn_timeout,
            cancel_grace: self.inner.cancel_grace,
            permission_policy: self.inner.config.permission_policy,
        }
    }

    fn resolve_bin(&self) -> Result<PathBuf> {
        self.inner.config.resolved_bin().ok_or_else(|| {
            anyhow::anyhow!(
                "no official ACP binary configured: set `[acp_runner] bin = \"...\"` in \
                 config.toml or set `BOARD_ACP_BIN`"
            )
        })
    }

    fn lookup_slot(&self, native_id: &str) -> Option<Arc<SessionSlot>> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(native_id)
            .cloned()
    }

    fn insert_slot(&self, key: String, slot: Arc<SessionSlot>) {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, slot);
    }

    fn recover_slot_from_db(&self, handle: &RunHandle) -> Result<Option<Arc<SessionSlot>>> {
        let Some(rec) = self
            .inner
            .local_db
            .get_local_run_by_session(&handle.native_id)?
        else {
            return Ok(None);
        };
        if rec.runner != self.kind() {
            return Ok(None);
        }
        let workdir = handle
            .workdir
            .clone()
            .or(rec.worktree_path.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let live_pre_restart = rec.pid.filter(|&pid| process_group_alive(Some(pid)));
        let (state, runnable, busy, pid, pgid) = match rec.state {
            LocalRunState::Cancelled => (RunnerState::Cancelled, false, false, None, None),
            LocalRunState::Failed => (RunnerState::Failed, false, false, None, None),
            LocalRunState::Running | LocalRunState::Starting | LocalRunState::Intent
                if live_pre_restart.is_some() =>
            {
                // A process from before restart is still running in its own process group.
                // Keep its pgid so cancel() terminates it cleanly, and refuse concurrent
                // respawn in send() until it finishes or is cancelled.
                (
                    RunnerState::Running,
                    false,
                    true,
                    live_pre_restart,
                    live_pre_restart,
                )
            }
            LocalRunState::Finished
            | LocalRunState::Running
            | LocalRunState::Starting
            | LocalRunState::Intent => (RunnerState::Idle, true, false, None, None),
        };
        let active_session_id = rec.session_id.unwrap_or_else(|| handle.native_id.clone());
        // Preserve strictly monotonic event sequence numbers across daemon restarts so
        // post-restart events are never filtered out by `ManagedRun::last_event_seq` or
        // dropped by `ON CONFLICT(run_id, seq) DO NOTHING` on the server.
        let persisted_seq = self
            .inner
            .local_db
            .load_watch_state()
            .ok()
            .and_then(|ws| {
                rec.run_id
                    .parse::<board_proto::RunId>()
                    .ok()
                    .and_then(|rid| ws.managed_runs.get(&rid).map(|m| m.last_event_seq))
            })
            .unwrap_or(0);
        let slot = Arc::new(SessionSlot {
            shared: Mutex::new(SessionShared {
                run_id: rec.run_id,
                handle_native_id: handle.native_id.clone(),
                active_session_id: active_session_id.clone(),
                workdir,
                model: None,
                state,
                runnable,
                busy,
                runs: 1,
                pid,
                pgid,
                next_seq: persisted_seq.saturating_add(1).max(1),
                events: VecDeque::new(),
            }),
            control: AsyncMutex::new(SessionControl::default()),
        });
        let mut guard = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = guard
            .entry(handle.native_id.clone())
            .or_insert_with(|| Arc::clone(&slot));
        let chosen = Arc::clone(entry);
        if active_session_id != handle.native_id {
            guard
                .entry(active_session_id)
                .or_insert_with(|| Arc::clone(&chosen));
        }
        Ok(Some(chosen))
    }
}

struct SessionSlot {
    shared: Mutex<SessionShared>,
    control: AsyncMutex<SessionControl>,
}

struct SessionShared {
    run_id: String,
    handle_native_id: String,
    active_session_id: String,
    workdir: PathBuf,
    model: Option<ThreadModel>,
    state: RunnerState,
    runnable: bool,
    busy: bool,
    runs: u64,
    pid: Option<u32>,
    pgid: Option<u32>,
    next_seq: u64,
    events: VecDeque<RunEvent>,
}

impl SessionShared {
    fn push_event(
        &mut self,
        at: Timestamp,
        kind: RunEventKind,
        summary: impl AsRef<str>,
        mut metadata: BTreeMap<String, String>,
    ) -> RunEvent {
        let seq = self.next_seq;
        self.next_seq += 1;
        metadata
            .entry("run_id".to_owned())
            .or_insert_with(|| self.run_id.clone());
        metadata
            .entry("session_id".to_owned())
            .or_insert_with(|| self.active_session_id.clone());
        let event = RunEvent {
            seq,
            at,
            kind,
            summary: clip_event_summary(summary.as_ref()),
            metadata,
        };
        if self.events.len() >= MAX_IN_MEMORY_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        event
    }

    fn push_user_message(&mut self, at: Timestamp, msg: &SteerMessage) -> RunEvent {
        let mut meta = BTreeMap::new();
        meta.insert("role".to_owned(), "user".to_owned());
        if let Some(digest) = msg
            .digest
            .as_deref()
            .or_else(|| extract_wake_digest(&msg.text))
        {
            meta.insert("wake_digest".to_owned(), digest.to_owned());
        }
        self.push_event(at, RunEventKind::Message, &msg.text, meta)
    }

    fn status(&self) -> RunnerStatus {
        RunnerStatus {
            state: self.state,
            runnable: self.runnable,
            busy: self.busy,
            runtime: Some(Runtime::Antigravity),
            workdir: Some(self.workdir.clone()),
            runs: Some(self.runs),
        }
    }
}

#[derive(Default)]
struct SessionControl {
    cmd_tx: Option<mpsc::UnboundedSender<SupervisorCommand>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

enum SendAttempt {
    Delivered,
    ProcessGone,
    Rejected(String),
}

enum SupervisorCommand {
    SendPrompt {
        msg: SteerMessage,
        ack: oneshot::Sender<SendAttempt>,
    },
    Cancel {
        reason: String,
        done: oneshot::Sender<()>,
    },
}

struct SpawnedProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    pid: Option<u32>,
    pgid: Option<u32>,
}

impl AcpRunner {
    fn spawn_child(&self, workdir: &Path, run_id: &str) -> Result<SpawnedProcess> {
        let bin = self.resolve_bin()?;
        let use_systemd = cfg!(target_os = "linux") && self.inner.config.cgroup.enabled;
        let launch = build_launch_command(
            &bin,
            &self.inner.config.args,
            &self.inner.config.cgroup,
            use_systemd,
        );

        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args)
            .current_dir(workdir)
            .envs(&self.inner.config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning official ACP binary {:?} in {}",
                launch.program,
                workdir.display()
            )
        })?;
        let pid = child.id();
        #[cfg(unix)]
        let pgid = pid;
        #[cfg(not(unix))]
        let pgid = None;

        let stdin = child.stdin.take().context("ACP child stdin not piped")?;
        let stdout = BufReader::new(child.stdout.take().context("ACP child stdout not piped")?);
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_STDERR_TAIL)));

        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            let home = self.inner.home.clone();
            let clock = Arc::clone(&self.inner.clock);
            let run_id = run_id.to_owned();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    {
                        let mut guard = tail.lock().unwrap_or_else(PoisonError::into_inner);
                        if guard.len() >= MAX_STDERR_TAIL {
                            guard.pop_front();
                        }
                        guard.push_back(trimmed.to_owned());
                    }
                    let _ = append_transcript(
                        &home,
                        &run_id,
                        (clock)(),
                        "stderr",
                        None,
                        json!({ "line": trimmed }),
                    );
                }
            });
        }

        Ok(SpawnedProcess {
            child,
            stdin,
            stdout,
            stderr_tail,
            pid,
            pgid,
        })
    }

    async fn rpc_initialize(
        &self,
        proc: &mut SpawnedProcess,
        run_id: &str,
        rpc_id: u64,
        handshake_events: &mut Vec<PendingRunEvent>,
    ) -> Result<bool> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "agent-board",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        });
        write_json_line(&mut proc.stdin, &req).await?;
        let _ = append_transcript(
            &self.inner.home,
            run_id,
            (self.inner.clock)(),
            "client",
            None,
            req,
        );
        let resp = read_rpc_response(
            &mut proc.stdout,
            &mut proc.stdin,
            rpc_id,
            self.inner.handshake_timeout,
            &self.inner.home,
            run_id,
            &self.inner.clock,
            None,
            &proc.stderr_tail,
            self.inner.config.permission_policy,
            handshake_events,
        )
        .await
        .context("ACP initialize handshake failed")?;
        let result = extract_rpc_result(&resp, "initialize")?;
        let supports_load = result
            .get("agentCapabilities")
            .and_then(|c| c.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok(supports_load)
    }

    async fn rpc_session_new(
        &self,
        proc: &mut SpawnedProcess,
        run_id: &str,
        workdir: &Path,
        spec_model: Option<&ThreadModel>,
        rpc_id: u64,
        handshake_events: &mut Vec<PendingRunEvent>,
    ) -> Result<String> {
        let mut params = Map::new();
        params.insert("cwd".to_owned(), json!(workdir.to_string_lossy()));
        params.insert("mcpServers".to_owned(), json!([]));

        let model = spec_model
            .map(|m| m.model.as_str())
            .or(self.inner.config.model.as_deref());
        if let Some(model) = model {
            params.insert("model".to_owned(), json!(model));
        }

        let mut options = self.inner.config.options.clone();
        if let Some(tm) = spec_model {
            for (k, v) in &tm.options {
                options.insert(k.clone(), v.clone());
            }
        }
        if !options.is_empty() {
            params.insert("options".to_owned(), json!(options));
        }

        let req = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "session/new",
            "params": Value::Object(params),
        });
        write_json_line(&mut proc.stdin, &req).await?;
        let _ = append_transcript(
            &self.inner.home,
            run_id,
            (self.inner.clock)(),
            "client",
            None,
            req,
        );
        let resp = read_rpc_response(
            &mut proc.stdout,
            &mut proc.stdin,
            rpc_id,
            self.inner.handshake_timeout,
            &self.inner.home,
            run_id,
            &self.inner.clock,
            None,
            &proc.stderr_tail,
            self.inner.config.permission_policy,
            handshake_events,
        )
        .await
        .context("ACP session/new failed")?;
        let result = extract_rpc_result(&resp, "session/new")?;
        let session_id = result
            .get("sessionId")
            .or_else(|| result.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("ACP session/new response missing non-empty sessionId")?;
        Ok(session_id.to_owned())
    }

    async fn rpc_session_load(
        &self,
        proc: &mut SpawnedProcess,
        run_id: &str,
        session_id: &str,
        workdir: &Path,
        rpc_id: u64,
        handshake_events: &mut Vec<PendingRunEvent>,
    ) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "session/load",
            "params": {
                "sessionId": session_id,
                "cwd": workdir.to_string_lossy(),
                "mcpServers": [],
            }
        });
        write_json_line(&mut proc.stdin, &req).await?;
        let _ = append_transcript(
            &self.inner.home,
            run_id,
            (self.inner.clock)(),
            "client",
            Some(session_id),
            req,
        );
        let resp = read_rpc_response(
            &mut proc.stdout,
            &mut proc.stdin,
            rpc_id,
            self.inner.handshake_timeout,
            &self.inner.home,
            run_id,
            &self.inner.clock,
            Some(session_id),
            &proc.stderr_tail,
            self.inner.config.permission_policy,
            handshake_events,
        )
        .await
        .context("ACP session/load failed")?;
        let _ = extract_rpc_result(&resp, "session/load")?;
        Ok(())
    }

    fn persist_spawn_in_db(
        &self,
        spec: &RunSpec,
        workdir: &Path,
        session_id: &str,
        pid: Option<u32>,
        now: Timestamp,
    ) -> Result<()> {
        if self.inner.local_db.get_local_run(&spec.run_id)?.is_some() {
            self.inner.local_db.update_local_run_runtime(
                &spec.run_id,
                LocalRunState::Running,
                Some(session_id),
                pid,
                false,
                now,
            )?;
            return Ok(());
        }
        let board = spec
            .metadata
            .get("board")
            .and_then(|raw| raw.parse::<BoardSlug>().ok())
            .unwrap_or_else(|| "local".parse().expect("valid fallback board slug"));
        let task_id = spec
            .metadata
            .get("task_id")
            .and_then(|raw| raw.parse().ok());
        let kind = spec
            .metadata
            .get("kind")
            .cloned()
            .unwrap_or_else(|| "worker".to_owned());
        let branch = spec.metadata.get("branch").cloned();
        let cost_units = spec
            .metadata
            .get("cost_units")
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(2);
        let meta_json = (!spec.metadata.is_empty())
            .then(|| serde_json::to_string(&spec.metadata).ok())
            .flatten();
        let record = LocalRunRecord {
            run_id: spec.run_id.clone(),
            task_id,
            board,
            kind,
            runner: self.kind().to_owned(),
            session_id: Some(session_id.to_owned()),
            worktree_path: Some(workdir.to_path_buf()),
            branch,
            cost_units,
            pid,
            state: LocalRunState::Running,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
            meta_json,
        };
        self.inner.local_db.upsert_local_run(&record)
    }

    fn launch_supervisor(
        &self,
        slot: Arc<SessionSlot>,
        proc: SpawnedProcess,
        initial_prompt_id: u64,
        next_rpc_id: u64,
    ) -> (
        mpsc::UnboundedSender<SupervisorCommand>,
        tokio::task::JoinHandle<()>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let ctx = self.supervisor_ctx();
        let task = tokio::spawn(async move {
            run_supervisor(ctx, slot, proc, initial_prompt_id, next_rpc_id, cmd_rx).await;
        });
        (cmd_tx, task)
    }
}

impl Runner for AcpRunner {
    fn kind(&self) -> &'static str {
        "acp"
    }

    fn spawn<'a>(
        &'a self,
        spec: &'a RunSpec,
        workdir: &'a Path,
    ) -> BoxFuture<'a, Result<RunHandle>> {
        Box::pin(async move {
            let mut proc = self.spawn_child(workdir, &spec.run_id)?;
            let mut handshake_events = Vec::new();
            let setup = async {
                let _supports_load = self
                    .rpc_initialize(&mut proc, &spec.run_id, 1, &mut handshake_events)
                    .await?;
                let session_id = self
                    .rpc_session_new(
                        &mut proc,
                        &spec.run_id,
                        workdir,
                        spec.model.as_ref(),
                        2,
                        &mut handshake_events,
                    )
                    .await?;
                let now = (self.inner.clock)();
                self.persist_spawn_in_db(spec, workdir, &session_id, proc.pid, now)?;
                send_session_prompt(
                    &mut proc.stdin,
                    &self.inner.home,
                    &spec.run_id,
                    &session_id,
                    3,
                    &spec.prompt,
                    &self.inner.clock,
                )
                .await?;
                Ok::<(String, Timestamp), anyhow::Error>((session_id, now))
            }
            .await;

            let (session_id, now) = match setup {
                Ok(pair) => pair,
                Err(err) => {
                    let _ = teardown_process_group(
                        &mut proc.child,
                        proc.pgid,
                        self.inner.cancel_grace.min(Duration::from_secs(2)),
                    )
                    .await;
                    return Err(err);
                }
            };

            let slot = Arc::new(SessionSlot {
                shared: Mutex::new(SessionShared {
                    run_id: spec.run_id.clone(),
                    handle_native_id: session_id.clone(),
                    active_session_id: session_id.clone(),
                    workdir: workdir.to_path_buf(),
                    model: spec.model.clone(),
                    state: RunnerState::Running,
                    runnable: true,
                    busy: true,
                    runs: 1,
                    pid: proc.pid,
                    pgid: proc.pgid,
                    next_seq: 1,
                    events: VecDeque::new(),
                }),
                control: AsyncMutex::new(SessionControl::default()),
            });

            {
                let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.push_event(
                    now,
                    RunEventKind::Phase,
                    format!("started {}", spec.title),
                    BTreeMap::new(),
                );
                for ev in handshake_events {
                    shared.push_event(ev.at, ev.kind, ev.summary, ev.metadata);
                }
                shared.push_user_message(now, &SteerMessage::new(&spec.prompt));
            }

            let (cmd_tx, supervisor) = self.launch_supervisor(Arc::clone(&slot), proc, 3, 4);
            {
                let mut control = slot.control.lock().await;
                control.cmd_tx = Some(cmd_tx);
                control.supervisor = Some(supervisor);
            }
            self.insert_slot(session_id.clone(), slot);

            let mut handle = RunHandle::new(self.kind(), &session_id).with_workdir(workdir);
            if let Ok(agent_id) = handle.resolve_agent_id() {
                handle = handle.with_agent_id(agent_id);
            }
            Ok(handle)
        })
    }

    fn send<'a>(
        &'a self,
        handle: &'a RunHandle,
        msg: &'a SteerMessage,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let slot = match self.lookup_slot(&handle.native_id) {
                Some(slot) => slot,
                None => self.recover_slot_from_db(handle)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "ACP session {:?} is not known locally or in daemon.db",
                        handle.native_id
                    )
                })?,
            };

            let mut control = slot.control.lock().await;

            // Reject terminal runs before touching the live supervisor channel.
            {
                let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                if matches!(shared.state, RunnerState::Cancelled | RunnerState::Failed)
                    || (!shared.runnable && control.cmd_tx.is_none())
                {
                    bail!(
                        "ACP session {} is not runnable (state: {:?})",
                        handle.native_id,
                        shared.state
                    );
                }
            }

            // Fast path: live supervisor process.
            if let Some(tx) = control.cmd_tx.clone() {
                let (ack_tx, ack_rx) = oneshot::channel();
                if tx
                    .send(SupervisorCommand::SendPrompt {
                        msg: msg.clone(),
                        ack: ack_tx,
                    })
                    .is_ok()
                {
                    match ack_rx.await {
                        Ok(SendAttempt::Delivered) => return Ok(()),
                        Ok(SendAttempt::Rejected(reason)) => bail!("{reason}"),
                        Ok(SendAttempt::ProcessGone) | Err(_) => {
                            // Fall through to await supervisor exit and respawn if idle.
                        }
                    }
                }
            }

            // Reap previous supervisor task if it just exited so shared state is final.
            control.cmd_tx = None;
            if let Some(task) = control.supervisor.take() {
                let _ = task.await;
            }

            let (run_id, prev_session_id, workdir, model, state, runnable, existing_pgid) = {
                let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                (
                    shared.run_id.clone(),
                    shared.active_session_id.clone(),
                    handle
                        .workdir
                        .clone()
                        .unwrap_or_else(|| shared.workdir.clone()),
                    shared.model.clone(),
                    shared.state,
                    shared.runnable,
                    shared.pgid,
                )
            };

            if process_group_alive(existing_pgid) {
                bail!(
                    "ACP session {} still has a live pre-restart process group ({:?}); cancel it \
                     or wait for it to exit before resuming",
                    handle.native_id,
                    existing_pgid
                );
            }
            if !runnable || matches!(state, RunnerState::Cancelled | RunnerState::Failed) {
                bail!(
                    "ACP session {} is not runnable (state: {:?})",
                    handle.native_id,
                    state
                );
            }
            if !workdir.exists() {
                bail!(
                    "cannot resume ACP session {}: workdir {} does not exist",
                    handle.native_id,
                    workdir.display()
                );
            }

            // Respawn official binary and resume via session/load (writing a Trail Rule record
            // BEFORE falling back to session/new when session/load is unsupported).
            let mut proc = self.spawn_child(&workdir, &run_id)?;
            let mut handshake_events = Vec::new();
            let resume_result = async {
                let supports_load = self
                    .rpc_initialize(&mut proc, &run_id, 1, &mut handshake_events)
                    .await?;
                let mut next_id = 2u64;
                let (active_session_id, fallback_info) = if supports_load {
                    match self
                        .rpc_session_load(
                            &mut proc,
                            &run_id,
                            &prev_session_id,
                            &workdir,
                            next_id,
                            &mut handshake_events,
                        )
                        .await
                    {
                        Ok(()) => {
                            next_id += 1;
                            (prev_session_id.clone(), None)
                        }
                        Err(err) => {
                            next_id += 1;
                            let err_text = format!("{err:#}");
                            let trail_now = (self.inner.clock)();
                            // Trail Rule: write fallback record BEFORE acting on session/new.
                            let pre_record = json!({
                                "at": trail_now,
                                "runner": self.kind(),
                                "run_id": run_id,
                                "previous_session_id": prev_session_id,
                                "workdir": workdir,
                                "reason": "session_load_unsupported",
                                "error": err_text,
                            });
                            let trail_path =
                                state::append_acp_fallback(&self.inner.home, &pre_record)
                                    .context("recording ACP session/load fallback trail entry")?;
                            let new_id = self
                                .rpc_session_new(
                                    &mut proc,
                                    &run_id,
                                    &workdir,
                                    model.as_ref(),
                                    next_id,
                                    &mut handshake_events,
                                )
                                .await?;
                            next_id += 1;
                            (new_id, Some((err_text, trail_path, trail_now)))
                        }
                    }
                } else {
                    let err_text =
                        "agent declared agentCapabilities.loadSession = false".to_owned();
                    let trail_now = (self.inner.clock)();
                    let pre_record = json!({
                        "at": trail_now,
                        "runner": self.kind(),
                        "run_id": run_id,
                        "previous_session_id": prev_session_id,
                        "workdir": workdir,
                        "reason": "session_load_unsupported",
                        "error": err_text,
                    });
                    let trail_path = state::append_acp_fallback(&self.inner.home, &pre_record)
                        .context("recording ACP session/load fallback trail entry")?;
                    let new_id = self
                        .rpc_session_new(
                            &mut proc,
                            &run_id,
                            &workdir,
                            model.as_ref(),
                            next_id,
                            &mut handshake_events,
                        )
                        .await?;
                    next_id += 1;
                    (new_id, Some((err_text, trail_path, trail_now)))
                };

                let now = (self.inner.clock)();
                if active_session_id != prev_session_id {
                    self.inner.local_db.record_local_run_session_alias(
                        &run_id,
                        &prev_session_id,
                        now,
                    )?;
                }
                self.inner.local_db.update_local_run_runtime(
                    &run_id,
                    LocalRunState::Running,
                    Some(&active_session_id),
                    proc.pid,
                    false,
                    now,
                )?;

                let prompt_id = next_id;
                send_session_prompt(
                    &mut proc.stdin,
                    &self.inner.home,
                    &run_id,
                    &active_session_id,
                    prompt_id,
                    &msg.text,
                    &self.inner.clock,
                )
                .await?;
                Ok::<
                    (
                        String,
                        Option<(String, PathBuf, Timestamp)>,
                        u64,
                        u64,
                        Timestamp,
                    ),
                    anyhow::Error,
                >((
                    active_session_id,
                    fallback_info,
                    prompt_id,
                    prompt_id + 1,
                    now,
                ))
            }
            .await;

            let (active_session_id, fallback_info, prompt_id, next_rpc_id, now) =
                match resume_result {
                    Ok(res) => res,
                    Err(err) => {
                        if !handshake_events.is_empty() {
                            let mut shared =
                                slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                            for ev in handshake_events {
                                shared.push_event(ev.at, ev.kind, ev.summary, ev.metadata);
                            }
                        }
                        let _ = teardown_process_group(
                            &mut proc.child,
                            proc.pgid,
                            self.inner.cancel_grace.min(Duration::from_secs(2)),
                        )
                        .await;
                        return Err(err);
                    }
                };

            if let Some((reason, trail_path, trail_now)) = fallback_info {
                let transcript_record = json!({
                    "at": trail_now,
                    "runner": self.kind(),
                    "run_id": run_id,
                    "previous_session_id": prev_session_id,
                    "new_session_id": active_session_id,
                    "workdir": workdir,
                    "reason": "session_load_unsupported",
                    "error": reason,
                    "trail_path": trail_path,
                });
                let _ = append_transcript(
                    &self.inner.home,
                    &run_id,
                    now,
                    "runner",
                    Some(&active_session_id),
                    transcript_record,
                );
                let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.active_session_id = active_session_id.clone();
                let mut meta = BTreeMap::new();
                meta.insert("fallback".to_owned(), "session_load_unsupported".to_owned());
                meta.insert("previous_session_id".to_owned(), prev_session_id.clone());
                meta.insert("trail_path".to_owned(), trail_path.display().to_string());
                shared.push_event(
                    now,
                    RunEventKind::Note,
                    format!(
                        "session/load unsupported for {prev_session_id}; started replacement session {active_session_id}: {reason}"
                    ),
                    meta,
                );
            } else {
                let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                let mut meta = BTreeMap::new();
                meta.insert("resumed".to_owned(), "session_load".to_owned());
                shared.push_event(
                    now,
                    RunEventKind::Phase,
                    format!("resumed session {active_session_id}"),
                    meta,
                );
            }

            {
                let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.active_session_id = active_session_id.clone();
                shared.state = RunnerState::Running;
                shared.runnable = true;
                shared.busy = true;
                shared.runs += 1;
                shared.pid = proc.pid;
                shared.pgid = proc.pgid;
                for ev in handshake_events {
                    shared.push_event(ev.at, ev.kind, ev.summary, ev.metadata);
                }
                shared.push_user_message(now, msg);
            }

            if active_session_id != handle.native_id {
                self.insert_slot(active_session_id, Arc::clone(&slot));
            }

            let (cmd_tx, supervisor) =
                self.launch_supervisor(Arc::clone(&slot), proc, prompt_id, next_rpc_id);
            control.cmd_tx = Some(cmd_tx);
            control.supervisor = Some(supervisor);
            Ok(())
        })
    }

    fn status<'a>(&'a self, handle: &'a RunHandle) -> BoxFuture<'a, Result<RunnerStatus>> {
        Box::pin(async move {
            if let Some(slot) = self.lookup_slot(&handle.native_id) {
                let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                return Ok(shared.status());
            }
            if let Some(slot) = self.recover_slot_from_db(handle)? {
                let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                return Ok(shared.status());
            }
            Ok(RunnerStatus {
                state: RunnerState::Gone,
                runnable: false,
                busy: false,
                runtime: None,
                workdir: None,
                runs: None,
            })
        })
    }

    fn cancel<'a>(&'a self, handle: &'a RunHandle, reason: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let now = (self.inner.clock)();
            let slot = match self.lookup_slot(&handle.native_id) {
                Some(slot) => Some(slot),
                None => self.recover_slot_from_db(handle)?,
            };

            let Some(slot) = slot else {
                record_cancel_trail(
                    &self.inner.home,
                    now,
                    self.kind(),
                    handle,
                    reason,
                    "already_ended",
                    true,
                )?;
                return Ok(());
            };

            // Trail Rule: persist cancellation record BEFORE sending session/cancel or signals.
            record_cancel_trail(
                &self.inner.home,
                now,
                self.kind(),
                handle,
                reason,
                "cancelled",
                true,
            )?;

            let mut control = slot.control.lock().await;
            if let Some(tx) = control.cmd_tx.take() {
                let (done_tx, done_rx) = oneshot::channel();
                if tx
                    .send(SupervisorCommand::Cancel {
                        reason: reason.to_owned(),
                        done: done_tx,
                    })
                    .is_ok()
                {
                    let _ = done_rx.await;
                }
            }
            if let Some(task) = control.supervisor.take() {
                let _ = task.await;
            }

            // If this session was recovered from LocalDb with a still-live pre-restart pgid,
            // terminate that process group cleanly too.
            let orphan_pgid = {
                let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.pgid
            };
            if process_group_alive(orphan_pgid) {
                terminate_detached_process_group(orphan_pgid, self.inner.cancel_grace).await;
            }

            // Ensure state + LocalDb reflect cancellation even if process had already exited idle.
            let run_id = {
                let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                let already_cancelled = shared.state == RunnerState::Cancelled;
                shared.state = RunnerState::Cancelled;
                shared.runnable = false;
                shared.busy = false;
                shared.pid = None;
                shared.pgid = None;
                if !already_cancelled {
                    let mut meta = BTreeMap::new();
                    meta.insert("reason".to_owned(), reason.to_owned());
                    shared.push_event(now, RunEventKind::Cancelled, reason, meta);
                }
                shared.run_id.clone()
            };
            self.inner.local_db.update_local_run_runtime(
                &run_id,
                LocalRunState::Cancelled,
                None,
                None,
                true,
                (self.inner.clock)(),
            )?;
            Ok(())
        })
    }

    fn poll_events<'a>(
        &'a self,
        handle: &'a RunHandle,
        after_seq: u64,
    ) -> BoxFuture<'a, Result<Vec<RunEvent>>> {
        Box::pin(async move {
            let Some(slot) = self.lookup_slot(&handle.native_id) else {
                return Ok(Vec::new());
            };
            let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(shared
                .events
                .iter()
                .filter(|ev| ev.seq > after_seq)
                .cloned()
                .collect())
        })
    }
}

async fn run_supervisor(
    ctx: SupervisorContext,
    slot: Arc<SessionSlot>,
    mut proc: SpawnedProcess,
    initial_prompt_id: u64,
    mut next_rpc_id: u64,
    mut cmd_rx: mpsc::UnboundedReceiver<SupervisorCommand>,
) {
    let (run_id, session_id, handle) = {
        let shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
        (
            shared.run_id.clone(),
            shared.active_session_id.clone(),
            RunHandle::new("acp", &shared.handle_native_id).with_workdir(&shared.workdir),
        )
    };

    let mut in_flight_prompt_id: Option<u64> = Some(initial_prompt_id);
    let mut pending_prompts: VecDeque<SteerMessage> = VecDeque::new();
    let mut turn_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> =
        Some(Box::pin(tokio::time::sleep(ctx.turn_timeout)));
    let mut line_buf = String::new();

    loop {
        tokio::select! {
            read_res = proc.stdout.read_line(&mut line_buf) => {
                match read_res {
                    Ok(0) => {
                        // EOF on stdout: wait boundedly for the child to exit (while still
                        // honoring Cancel) and ensure any surviving grandchildren in proc.pgid
                        // are torn down so nothing leaks.
                        let wait_limit = ctx.cancel_grace.clamp(
                            Duration::from_millis(200),
                            Duration::from_secs(2),
                        );
                        let mut cancel_req: Option<(String, oneshot::Sender<()>)> = None;
                        let exit_status = tokio::select! {
                            res = tokio::time::timeout(wait_limit, proc.child.wait()) => {
                                res.ok().and_then(Result::ok)
                            }
                            cmd = cmd_rx.recv() => {
                                match cmd {
                                    Some(SupervisorCommand::Cancel { reason, done }) => {
                                        cancel_req = Some((reason, done));
                                    }
                                    Some(SupervisorCommand::SendPrompt { ack, .. }) => {
                                        let _ = ack.send(SendAttempt::ProcessGone);
                                    }
                                    None => {}
                                }
                                None
                            }
                        };

                        if let Some((reason, done)) = cancel_req {
                            record_dropped_prompts(
                                &ctx,
                                &slot,
                                &run_id,
                                &session_id,
                                "cancelled",
                                &mut pending_prompts,
                            );
                            let outcome = teardown_process_group(
                                &mut proc.child,
                                proc.pgid,
                                ctx.cancel_grace,
                            )
                            .await;
                            let now = (ctx.clock)();
                            {
                                let mut shared =
                                    slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                shared.state = RunnerState::Cancelled;
                                shared.runnable = false;
                                shared.busy = false;
                                shared.pid = None;
                                shared.pgid = None;
                                let mut meta = BTreeMap::new();
                                meta.insert("reason".to_owned(), reason.clone());
                                meta.insert("teardown".to_owned(), outcome.to_owned());
                                shared.push_event(now, RunEventKind::Cancelled, &reason, meta);
                            }
                            let _ = ctx.local_db.update_local_run_runtime(
                                &run_id,
                                LocalRunState::Cancelled,
                                None,
                                None,
                                true,
                                now,
                            );
                            let _ = done.send(());
                            break;
                        }

                        // Tear down any lingering child or surviving grandchildren in the group.
                        let had_lingering = exit_status.is_none() || process_group_alive(proc.pgid);
                        if had_lingering {
                            let _ = teardown_process_group(
                                &mut proc.child,
                                proc.pgid,
                                ctx.cancel_grace.min(Duration::from_secs(2)),
                            )
                            .await;
                        }

                        if !pending_prompts.is_empty() {
                            record_dropped_prompts(
                                &ctx,
                                &slot,
                                &run_id,
                                &session_id,
                                "process_exited_with_queued_prompts",
                                &mut pending_prompts,
                            );
                        }

                        let now = (ctx.clock)();
                        let final_db_state = {
                            let mut shared =
                                slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                            shared.pid = None;
                            shared.pgid = None;
                            if in_flight_prompt_id.is_some() {
                                let detail = format_exit_detail(exit_status, &proc.stderr_tail);
                                shared.push_event(
                                    now,
                                    RunEventKind::Error,
                                    format!("ACP process exited mid-turn ({detail})"),
                                    BTreeMap::new(),
                                );
                                shared.state = RunnerState::Failed;
                                shared.runnable = false;
                                shared.busy = false;
                                LocalRunState::Failed
                            } else if exit_status.is_some_and(|s| s.success()) {
                                // Clean idle exit: session remains resumable via session/load.
                                shared.state = RunnerState::Idle;
                                shared.runnable = true;
                                shared.busy = false;
                                LocalRunState::Finished
                            } else {
                                let detail = format_exit_detail(exit_status, &proc.stderr_tail);
                                shared.push_event(
                                    now,
                                    RunEventKind::Error,
                                    format!("ACP process exited unexpectedly ({detail})"),
                                    BTreeMap::new(),
                                );
                                shared.state = RunnerState::Failed;
                                shared.runnable = false;
                                shared.busy = false;
                                LocalRunState::Failed
                            }
                        };
                        let _ = ctx.local_db.update_local_run_runtime(
                            &run_id,
                            final_db_state,
                            None,
                            None,
                            true,
                            now,
                        );
                        break;
                    }
                    Ok(_) => {
                        let raw = line_buf.trim().to_owned();
                        line_buf.clear();
                        if raw.is_empty() {
                            continue;
                        }
                        let now = (ctx.clock)();
                        let parsed: Value = match serde_json::from_str(&raw) {
                            Ok(val) => val,
                            Err(_) => {
                                let _ = append_transcript(
                                    &ctx.home,
                                    &run_id,
                                    now,
                                    "stdout_raw",
                                    Some(&session_id),
                                    json!({ "line": raw }),
                                );
                                continue;
                            }
                        };
                        let _ = append_transcript(
                            &ctx.home,
                            &run_id,
                            now,
                            "agent",
                            Some(&session_id),
                            parsed.clone(),
                        );

                        let method = parsed.get("method").and_then(Value::as_str);
                        let rpc_id = parsed.get("id").and_then(Value::as_u64);

                        if let Some(method) = method {
                            if let Some(req_id) = parsed.get("id") {
                                if method == "session/request_permission" {
                                    let ev = handle_permission_request(
                                        &mut proc.stdin,
                                        &ctx.home,
                                        &run_id,
                                        Some(&session_id),
                                        req_id,
                                        parsed.get("params"),
                                        ctx.permission_policy,
                                        now,
                                    )
                                    .await;
                                    let mut shared =
                                        slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                    shared.push_event(ev.at, ev.kind, ev.summary, ev.metadata);
                                } else {
                                    // Agent-to-client JSON-RPC request: answer cleanly so the agent
                                    // never deadlocks waiting for an interactive UI prompt.
                                    let _ = decline_client_request(
                                        &mut proc.stdin,
                                        &ctx.home,
                                        &run_id,
                                        Some(&session_id),
                                        req_id,
                                        method,
                                        now,
                                    )
                                    .await;
                                    let mut shared =
                                        slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                    let mut meta = BTreeMap::new();
                                    meta.insert("client_method".to_owned(), method.to_owned());
                                    shared.push_event(
                                        now,
                                        RunEventKind::Note,
                                        format!(
                                            "declined unsupported agent client request `{method}`"
                                        ),
                                        meta,
                                    );
                                }
                            } else if method == "session/update"
                                && let Some(params) = parsed.get("params")
                            {
                                let (kind, summary, meta) = normalize_session_update(params);
                                let mut shared =
                                    slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                shared.push_event(now, kind, summary, meta);
                            }
                        } else if let Some(resp_id) = rpc_id
                            && in_flight_prompt_id == Some(resp_id)
                        {
                            in_flight_prompt_id = None;
                            turn_deadline = None;
                            if let Some(err_obj) = parsed.get("error") {
                                let code =
                                    err_obj.get("code").and_then(Value::as_i64).unwrap_or(-1);
                                let msg = err_obj
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("ACP session/prompt error");
                                {
                                    let mut shared =
                                        slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                    let mut meta = BTreeMap::new();
                                    meta.insert("rpc_error_code".to_owned(), code.to_string());
                                    shared.push_event(
                                        now,
                                        RunEventKind::Error,
                                        format!("ACP prompt error ({code}): {msg}"),
                                        meta,
                                    );
                                    shared.state = RunnerState::Failed;
                                    shared.runnable = false;
                                    shared.busy = false;
                                    shared.pid = None;
                                    shared.pgid = None;
                                }
                                record_dropped_prompts(
                                    &ctx,
                                    &slot,
                                    &run_id,
                                    &session_id,
                                    "rpc_error",
                                    &mut pending_prompts,
                                );
                                let _ = proc.stdin.shutdown().await;
                                let _ = teardown_process_group(
                                    &mut proc.child,
                                    proc.pgid,
                                    ctx.cancel_grace.min(Duration::from_secs(2)),
                                )
                                .await;
                                let _ = ctx.local_db.update_local_run_runtime(
                                    &run_id,
                                    LocalRunState::Failed,
                                    None,
                                    None,
                                    true,
                                    now,
                                );
                                break;
                            }

                            let stop_reason = parsed
                                .get("result")
                                .and_then(|r| {
                                    r.get("stopReason").or_else(|| r.get("stop_reason"))
                                })
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn");

                            {
                                let mut shared =
                                    slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                let mut meta = BTreeMap::new();
                                meta.insert("stop_reason".to_owned(), stop_reason.to_owned());
                                if stop_reason == "cancelled" {
                                    shared.push_event(
                                        now,
                                        RunEventKind::Cancelled,
                                        "turn cancelled",
                                        meta,
                                    );
                                } else {
                                    shared.push_event(
                                        now,
                                        RunEventKind::Done,
                                        format!("turn completed ({stop_reason})"),
                                        meta,
                                    );
                                }
                            }

                            if let Some(next_msg) = pending_prompts.pop_front() {
                                let id = next_rpc_id;
                                next_rpc_id += 1;
                                if send_session_prompt(
                                    &mut proc.stdin,
                                    &ctx.home,
                                    &run_id,
                                    &session_id,
                                    id,
                                    &next_msg.text,
                                    &ctx.clock,
                                )
                                .await
                                .is_ok()
                                {
                                    in_flight_prompt_id = Some(id);
                                    turn_deadline = Some(Box::pin(tokio::time::sleep(
                                        ctx.turn_timeout,
                                    )));
                                    let sent_at = (ctx.clock)();
                                    let mut shared = slot
                                        .shared
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner);
                                    shared.state = RunnerState::Running;
                                    shared.runnable = true;
                                    shared.busy = true;
                                    shared.runs += 1;
                                    shared.push_user_message(sent_at, &next_msg);
                                } else {
                                    // Write failed (e.g., process exited right after turn 1):
                                    // record Trail Rule drops for this and any remaining prompts.
                                    pending_prompts.push_front(next_msg);
                                    record_dropped_prompts(
                                        &ctx,
                                        &slot,
                                        &run_id,
                                        &session_id,
                                        "stdin_closed_before_queued_prompt",
                                        &mut pending_prompts,
                                    );
                                    let mut shared = slot
                                        .shared
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner);
                                    shared.state = RunnerState::Idle;
                                    shared.busy = false;
                                }
                            } else {
                                let mut shared =
                                    slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                                shared.state = RunnerState::Idle;
                                shared.runnable = true;
                                shared.busy = false;
                            }
                        }
                    }
                    Err(err) => {
                        let now = (ctx.clock)();
                        {
                            let mut shared =
                                slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                            shared.push_event(
                                now,
                                RunEventKind::Error,
                                format!("reading ACP stdout failed: {err}"),
                                BTreeMap::new(),
                            );
                            shared.state = RunnerState::Failed;
                            shared.runnable = false;
                            shared.busy = false;
                            shared.pid = None;
                            shared.pgid = None;
                        }
                        record_dropped_prompts(
                            &ctx,
                            &slot,
                            &run_id,
                            &session_id,
                            "stdout_read_error",
                            &mut pending_prompts,
                        );
                        let _ = teardown_process_group(
                            &mut proc.child,
                            proc.pgid,
                            ctx.cancel_grace.min(Duration::from_secs(2)),
                        )
                        .await;
                        let _ = ctx.local_db.update_local_run_runtime(
                            &run_id,
                            LocalRunState::Failed,
                            None,
                            None,
                            true,
                            now,
                        );
                        break;
                    }
                }
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SupervisorCommand::SendPrompt { msg, ack }) => {
                        let not_runnable = {
                            let shared =
                                slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                            matches!(shared.state, RunnerState::Cancelled | RunnerState::Failed)
                                || !shared.runnable
                        };
                        if not_runnable {
                            let _ = ack.send(SendAttempt::Rejected(format!(
                                "ACP session {session_id} is not runnable"
                            )));
                            continue;
                        }
                        if in_flight_prompt_id.is_some() {
                            pending_prompts.push_back(msg);
                            let _ = ack.send(SendAttempt::Delivered);
                            continue;
                        }
                        let id = next_rpc_id;
                        next_rpc_id += 1;
                        match send_session_prompt(
                            &mut proc.stdin,
                            &ctx.home,
                            &run_id,
                            &session_id,
                            id,
                            &msg.text,
                            &ctx.clock,
                        )
                        .await
                        {
                            Ok(()) => {
                                in_flight_prompt_id = Some(id);
                                turn_deadline = Some(Box::pin(tokio::time::sleep(
                                    ctx.turn_timeout,
                                )));
                                let now = (ctx.clock)();
                                {
                                    let mut shared = slot
                                        .shared
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner);
                                    shared.state = RunnerState::Running;
                                    shared.runnable = true;
                                    shared.busy = true;
                                    shared.runs += 1;
                                    shared.push_user_message(now, &msg);
                                }
                                let _ = ctx.local_db.update_local_run_runtime(
                                    &run_id,
                                    LocalRunState::Running,
                                    None,
                                    proc.pid,
                                    false,
                                    now,
                                );
                                let _ = ack.send(SendAttempt::Delivered);
                            }
                            Err(_) => {
                                let _ = ack.send(SendAttempt::ProcessGone);
                            }
                        }
                    }
                    Some(SupervisorCommand::Cancel { reason, done }) => {
                        record_dropped_prompts(
                            &ctx,
                            &slot,
                            &run_id,
                            &session_id,
                            "cancelled",
                            &mut pending_prompts,
                        );

                        let cancel_notif = json!({
                            "jsonrpc": "2.0",
                            "method": "session/cancel",
                            "params": {
                                "sessionId": session_id,
                            }
                        });
                        let _ = write_json_line(&mut proc.stdin, &cancel_notif).await;
                        let _ = append_transcript(
                            &ctx.home,
                            &run_id,
                            (ctx.clock)(),
                            "client",
                            Some(&session_id),
                            cancel_notif,
                        );
                        let _ = proc.stdin.shutdown().await;

                        let outcome =
                            teardown_process_group(&mut proc.child, proc.pgid, ctx.cancel_grace)
                                .await;
                        let now = (ctx.clock)();
                        let _ = append_transcript(
                            &ctx.home,
                            &run_id,
                            now,
                            "runner",
                            Some(&session_id),
                            json!({
                                "event": "cancelled",
                                "reason": reason,
                                "teardown": outcome,
                            }),
                        );
                        {
                            let mut shared =
                                slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                            shared.state = RunnerState::Cancelled;
                            shared.runnable = false;
                            shared.busy = false;
                            shared.pid = None;
                            shared.pgid = None;
                            let mut meta = BTreeMap::new();
                            meta.insert("reason".to_owned(), reason.clone());
                            meta.insert("teardown".to_owned(), outcome.to_owned());
                            shared.push_event(now, RunEventKind::Cancelled, &reason, meta);
                        }
                        let _ = ctx.local_db.update_local_run_runtime(
                            &run_id,
                            LocalRunState::Cancelled,
                            None,
                            None,
                            true,
                            now,
                        );
                        let _ = done.send(());
                        break;
                    }
                    None => {
                        // Control channel closed (runner dropped); clean up process group.
                        record_dropped_prompts(
                            &ctx,
                            &slot,
                            &run_id,
                            &session_id,
                            "runner_dropped",
                            &mut pending_prompts,
                        );
                        let _ = teardown_process_group(
                            &mut proc.child,
                            proc.pgid,
                            ctx.cancel_grace.min(Duration::from_secs(2)),
                        )
                        .await;
                        break;
                    }
                }
            }

            () = async {
                if let Some(sleep) = turn_deadline.as_mut() {
                    sleep.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let now = (ctx.clock)();
                let reason = format!(
                    "ACP turn exceeded {}ms timeout",
                    ctx.turn_timeout.as_millis()
                );
                // Trail Rule record BEFORE cancelling/killing on timeout.
                let _ = record_cancel_trail(
                    &ctx.home,
                    now,
                    "acp",
                    &handle,
                    &reason,
                    "turn_timeout",
                    true,
                );
                record_dropped_prompts(
                    &ctx,
                    &slot,
                    &run_id,
                    &session_id,
                    "turn_timeout",
                    &mut pending_prompts,
                );
                let cancel_notif = json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": {
                        "sessionId": session_id,
                    }
                });
                let _ = write_json_line(&mut proc.stdin, &cancel_notif).await;
                let _ = proc.stdin.shutdown().await;
                let outcome =
                    teardown_process_group(&mut proc.child, proc.pgid, ctx.cancel_grace).await;
                let _ = append_transcript(
                    &ctx.home,
                    &run_id,
                    now,
                    "runner",
                    Some(&session_id),
                    json!({
                        "event": "turn_timeout",
                        "reason": reason,
                        "teardown": outcome,
                    }),
                );
                {
                    let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
                    let mut meta = BTreeMap::new();
                    meta.insert("teardown".to_owned(), outcome.to_owned());
                    shared.push_event(now, RunEventKind::Error, &reason, meta);
                    shared.state = RunnerState::Failed;
                    shared.runnable = false;
                    shared.busy = false;
                    shared.pid = None;
                    shared.pgid = None;
                }
                let _ = ctx.local_db.update_local_run_runtime(
                    &run_id,
                    LocalRunState::Failed,
                    None,
                    None,
                    true,
                    now,
                );
                break;
            }
        }
    }
}

fn record_dropped_prompts(
    ctx: &SupervisorContext,
    slot: &SessionSlot,
    run_id: &str,
    session_id: &str,
    reason: &str,
    pending_prompts: &mut VecDeque<SteerMessage>,
) {
    while let Some(dropped) = pending_prompts.pop_front() {
        let now = (ctx.clock)();
        let record = json!({
            "at": now,
            "runner": "acp",
            "run_id": run_id,
            "session_id": session_id,
            "reason": reason,
            "digest": dropped.digest,
            "prompt": dropped.text,
        });
        let trail_path = state::append_acp_drop(&ctx.home, &record).ok();
        let _ = append_transcript(&ctx.home, run_id, now, "runner", Some(session_id), record);
        let mut shared = slot.shared.lock().unwrap_or_else(PoisonError::into_inner);
        let mut meta = BTreeMap::new();
        meta.insert("dropped_reason".to_owned(), reason.to_owned());
        if let Some(digest) = dropped.digest {
            meta.insert("wake_digest".to_owned(), digest);
        }
        if let Some(path) = trail_path {
            meta.insert("trail_path".to_owned(), path.display().to_string());
        }
        shared.push_event(
            now,
            RunEventKind::Error,
            format!("dropped queued ACP prompt ({reason})"),
            meta,
        );
    }
}

async fn send_session_prompt(
    stdin: &mut ChildStdin,
    home: &Home,
    run_id: &str,
    session_id: &str,
    rpc_id: u64,
    prompt_text: &str,
    clock: &Clock,
) -> Result<()> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [
                {
                    "type": "text",
                    "text": prompt_text,
                }
            ]
        }
    });
    write_json_line(stdin, &req).await?;
    let _ = append_transcript(home, run_id, (clock)(), "client", Some(session_id), req);
    Ok(())
}

struct PendingRunEvent {
    at: Timestamp,
    kind: RunEventKind,
    summary: String,
    metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PermissionOutcome {
    Selected {
        option_id: String,
        option_kind: &'static str,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionDecision {
    tool: String,
    tool_call_id: Option<String>,
    tool_kind: Option<String>,
    options_count: usize,
    outcome: PermissionOutcome,
}

fn first_non_empty_str<'a>(val: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        val.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}

fn decide_permission(policy: AcpPermissionPolicy, params: Option<&Value>) -> PermissionDecision {
    let tool_call = params.and_then(|p| {
        p.get("toolCall")
            .filter(|v| v.is_object())
            .or_else(|| p.get("tool_call").filter(|v| v.is_object()))
    });
    let tool_keys = ["title", "toolName", "tool_name", "name"];
    let tool = tool_call
        .and_then(|tc| first_non_empty_str(tc, &tool_keys))
        .or_else(|| params.and_then(|p| first_non_empty_str(p, &tool_keys)))
        .unwrap_or("unknown tool")
        .to_owned();

    let id_keys = ["toolCallId", "tool_call_id"];
    let tool_call_id = tool_call
        .and_then(|tc| first_non_empty_str(tc, &id_keys))
        .or_else(|| params.and_then(|p| first_non_empty_str(p, &id_keys)))
        .map(str::to_owned);

    let tool_kind = tool_call
        .and_then(|tc| first_non_empty_str(tc, &["kind"]))
        .map(str::to_owned);

    let raw_options = params
        .and_then(|p| p.get("options"))
        .and_then(Value::as_array);
    let options_count = raw_options.map_or(0, Vec::len);

    let parsed_options: Vec<(&str, &str)> = raw_options
        .into_iter()
        .flatten()
        .filter_map(|opt| {
            let option_id = first_non_empty_str(opt, &["optionId", "option_id"])?;
            let kind = first_non_empty_str(opt, &["kind"])?;
            Some((option_id, kind))
        })
        .collect();

    let preferred_kinds: &[&str] = match policy {
        AcpPermissionPolicy::Allow => &["allow_always", "allow_once"],
        AcpPermissionPolicy::Deny => &["reject_once", "reject_always"],
    };

    let outcome = preferred_kinds
        .iter()
        .find_map(|&want| {
            parsed_options
                .iter()
                .find(|(_, kind)| *kind == want)
                .map(|(option_id, _)| PermissionOutcome::Selected {
                    option_id: (*option_id).to_owned(),
                    option_kind: want,
                })
        })
        .unwrap_or(PermissionOutcome::Cancelled);

    PermissionDecision {
        tool,
        tool_call_id,
        tool_kind,
        options_count,
        outcome,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_permission_request(
    stdin: &mut ChildStdin,
    home: &Home,
    run_id: &str,
    session_id: Option<&str>,
    req_id: &Value,
    params: Option<&Value>,
    policy: AcpPermissionPolicy,
    at: Timestamp,
) -> PendingRunEvent {
    let decision = decide_permission(policy, params);
    let (outcome_label, option_id, option_kind, rpc_outcome) = match &decision.outcome {
        PermissionOutcome::Selected {
            option_id,
            option_kind,
        } => (
            "selected",
            Some(option_id.as_str()),
            Some(*option_kind),
            json!({
                "outcome": "selected",
                "optionId": option_id,
            }),
        ),
        PermissionOutcome::Cancelled => (
            "cancelled",
            None,
            None,
            json!({
                "outcome": "cancelled",
            }),
        ),
    };

    // Trail Rule: record every permission decision in the raw JSONL transcript BEFORE acting.
    let trail_record = json!({
        "event": "permission_decision",
        "method": "session/request_permission",
        "request_id": req_id,
        "policy": policy.as_str(),
        "tool": decision.tool,
        "tool_call_id": decision.tool_call_id,
        "tool_kind": decision.tool_kind,
        "outcome": outcome_label,
        "option_id": option_id,
        "option_kind": option_kind,
        "options_count": decision.options_count,
    });
    let _ = append_transcript(home, run_id, at, "runner", session_id, trail_record);

    let resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {
            "outcome": rpc_outcome,
        },
    });
    let write_err = match write_json_line(stdin, &resp).await {
        Ok(()) => {
            let _ = append_transcript(home, run_id, at, "client", session_id, resp);
            None
        }
        Err(err) => {
            tracing::warn!("writing ACP session/request_permission response for {run_id}: {err:#}");
            Some(format!("{err:#}"))
        }
    };

    let mut meta = BTreeMap::new();
    meta.insert(
        "client_method".to_owned(),
        "session/request_permission".to_owned(),
    );
    meta.insert("policy".to_owned(), policy.as_str().to_owned());
    meta.insert("tool".to_owned(), decision.tool.clone());
    meta.insert("outcome".to_owned(), outcome_label.to_owned());
    if let Some(id) = option_id {
        meta.insert("option_id".to_owned(), id.to_owned());
    }
    if let Some(kind) = option_kind {
        meta.insert("option_kind".to_owned(), kind.to_owned());
    }
    if let Some(tc_id) = &decision.tool_call_id {
        meta.insert("tool_call_id".to_owned(), tc_id.clone());
    }
    if let Some(tc_kind) = &decision.tool_kind {
        meta.insert("tool_kind".to_owned(), tc_kind.clone());
    }
    if let Some(err) = write_err {
        meta.insert("write_error".to_owned(), err);
    }

    let summary = match (option_id, option_kind) {
        (Some(id), Some(kind)) => format!(
            "permission {} for {}: selected {} ({})",
            policy.as_str(),
            decision.tool,
            id,
            kind
        ),
        _ => format!(
            "permission {} for {}: cancelled",
            policy.as_str(),
            decision.tool
        ),
    };

    PendingRunEvent {
        at,
        kind: RunEventKind::Note,
        summary,
        metadata: meta,
    }
}

async fn decline_client_request(
    stdin: &mut ChildStdin,
    home: &Home,
    run_id: &str,
    session_id: Option<&str>,
    req_id: &Value,
    method: &str,
    at: Timestamp,
) -> Result<()> {
    let err_resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {
            "code": -32601,
            "message": format!("client method `{method}` is not supported by headless AcpRunner"),
        }
    });
    write_json_line(stdin, &err_resp).await?;
    let _ = append_transcript(home, run_id, at, "client", session_id, err_resp);
    Ok(())
}

async fn write_json_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).context("serializing ACP JSON-RPC message")?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .context("writing ACP JSON-RPC message to stdin")?;
    stdin.flush().await.context("flushing ACP JSON-RPC stdin")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn read_rpc_response(
    stdout: &mut BufReader<ChildStdout>,
    stdin: &mut ChildStdin,
    expected_id: u64,
    timeout: Duration,
    home: &Home,
    run_id: &str,
    clock: &Clock,
    session_id: Option<&str>,
    stderr_tail: &Mutex<VecDeque<String>>,
    permission_policy: AcpPermissionPolicy,
    handshake_events: &mut Vec<PendingRunEvent>,
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut line = String::new();
    loop {
        line.clear();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!(
                "timed out after {}ms waiting for ACP response id={expected_id}{}",
                timeout.as_millis(),
                stderr_suffix(stderr_tail)
            );
        }
        let read = tokio::time::timeout(remaining, stdout.read_line(&mut line))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "timed out after {}ms waiting for ACP response id={expected_id}{}",
                    timeout.as_millis(),
                    stderr_suffix(stderr_tail)
                )
            })?
            .context("reading ACP stdout during handshake")?;
        if read == 0 {
            bail!(
                "ACP process closed stdout before answering id={expected_id}{}",
                stderr_suffix(stderr_tail)
            );
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let now = (clock)();
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            let _ = append_transcript(
                home,
                run_id,
                now,
                "stdout_raw",
                session_id,
                json!({ "line": trimmed }),
            );
            continue;
        };
        let _ = append_transcript(home, run_id, now, "agent", session_id, value.clone());
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if let Some(req_id) = value.get("id") {
                if method == "session/request_permission" {
                    let ev = handle_permission_request(
                        stdin,
                        home,
                        run_id,
                        session_id,
                        req_id,
                        value.get("params"),
                        permission_policy,
                        now,
                    )
                    .await;
                    handshake_events.push(ev);
                } else {
                    // Decline unsupported agent-to-client requests during handshake so the agent
                    // never deadlocks.
                    let _ = decline_client_request(
                        stdin, home, run_id, session_id, req_id, method, now,
                    )
                    .await;
                }
            }
            continue;
        }
        if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return Ok(value);
        }
    }
}

fn extract_rpc_result<'a>(resp: &'a Value, method: &str) -> Result<&'a Value> {
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error");
        bail!("ACP {method} returned error {code}: {msg}");
    }
    resp.get("result")
        .with_context(|| format!("ACP {method} response missing `result`"))
}

fn append_transcript(
    home: &Home,
    run_id: &str,
    at: Timestamp,
    dir: &str,
    session_id: Option<&str>,
    payload: Value,
) -> Result<PathBuf> {
    let record = json!({
        "at": at,
        "dir": dir,
        "run_id": run_id,
        "session_id": session_id,
        "payload": payload,
    });
    state::append_transcript_line(home, run_id, &record)
}

fn normalize_session_update(params: &Value) -> (RunEventKind, String, BTreeMap<String, String>) {
    let update = params.get("update").unwrap_or(params);
    let update_type = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .or_else(|| update.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("update");

    let mut meta = BTreeMap::new();
    meta.insert("update_type".to_owned(), update_type.to_owned());
    if let Some(tool_call_id) = update
        .get("toolCallId")
        .or_else(|| update.get("tool_call_id"))
        .and_then(Value::as_str)
    {
        meta.insert("tool_call_id".to_owned(), tool_call_id.to_owned());
    }
    if let Some(status) = update.get("status").and_then(Value::as_str) {
        meta.insert("status".to_owned(), status.to_owned());
    }
    if let Some(kind) = update.get("kind").and_then(Value::as_str)
        && matches!(
            update_type,
            "tool_call" | "tool_call_update" | "tool_result"
        )
    {
        meta.insert("tool_kind".to_owned(), kind.to_owned());
    }

    let kind = match update_type {
        "agent_thought_chunk" | "thought" | "plan" | "current_mode_update" | "phase" | "status" => {
            RunEventKind::Phase
        }
        "tool_call" | "tool_call_update" | "tool_result" => RunEventKind::Tool,
        "error" => RunEventKind::Error,
        "done" | "completed" | "turn_complete" => RunEventKind::Done,
        "agent_message_chunk" | "user_message_chunk" | "message" | "note" => RunEventKind::Note,
        _ => RunEventKind::Note,
    };

    let summary = extract_update_summary(update_type, update);
    (kind, summary, meta)
}

fn extract_update_summary(update_type: &str, update: &Value) -> String {
    if matches!(
        update_type,
        "tool_call" | "tool_call_update" | "tool_result"
    ) {
        let title = update
            .get("title")
            .or_else(|| update.get("toolName"))
            .or_else(|| update.get("name"))
            .and_then(Value::as_str);
        let status = update.get("status").and_then(Value::as_str);
        let content_text = extract_content_text(update);
        return match (title, status, content_text) {
            (Some(t), Some(s), Some(c)) if !c.is_empty() => format!("{t} ({s}): {c}"),
            (Some(t), Some(s), _) => format!("{t} ({s})"),
            (Some(t), None, Some(c)) if !c.is_empty() => format!("{t}: {c}"),
            (Some(t), None, _) => t.to_owned(),
            (None, Some(s), Some(c)) if !c.is_empty() => format!("{update_type} ({s}): {c}"),
            (None, Some(s), _) => format!("{update_type} ({s})"),
            (None, None, Some(c)) if !c.is_empty() => c,
            _ => update_type.to_owned(),
        };
    }

    if let Some(text) = extract_content_text(update)
        && !text.is_empty()
    {
        return text;
    }
    if let Some(entries) = update.get("entries").and_then(Value::as_array) {
        let joined: Vec<&str> = entries
            .iter()
            .filter_map(|entry| entry.get("content").and_then(Value::as_str))
            .collect();
        if !joined.is_empty() {
            return joined.join("; ");
        }
    }
    if let Some(title) = update.get("title").and_then(Value::as_str)
        && !title.is_empty()
    {
        return title.to_owned();
    }
    if let Some(mode) = update.get("currentModeId").and_then(Value::as_str) {
        return format!("mode: {mode}");
    }
    if let Some(status) = update.get("status").and_then(Value::as_str) {
        return status.to_owned();
    }
    update_type.to_owned()
}

fn extract_content_text(update: &Value) -> Option<String> {
    if let Some(text) = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    if let Some(arr) = update.get("content").and_then(Value::as_array) {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content").and_then(|c| c.get("text")))
                    .and_then(Value::as_str)
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    for key in ["text", "message", "summary", "output"] {
        if let Some(text) = update.get(key).and_then(Value::as_str) {
            return Some(text.to_owned());
        }
    }
    None
}

fn stderr_suffix(stderr_tail: &Mutex<VecDeque<String>>) -> String {
    let guard = stderr_tail.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_empty() {
        String::new()
    } else {
        let lines: Vec<&str> = guard.iter().map(String::as_str).collect();
        format!("; stderr: {}", lines.join(" | "))
    }
}

fn format_exit_detail(
    status: Option<std::process::ExitStatus>,
    stderr_tail: &Mutex<VecDeque<String>>,
) -> String {
    let base = match status {
        Some(s) => s.to_string(),
        None => "unknown exit status".to_owned(),
    };
    format!("{base}{}", stderr_suffix(stderr_tail))
}

/// Gracefully waits up to `grace` for `child` (and its Unix process group `pgid`) to exit,
/// then escalates to `SIGTERM` across the process group, and finally `SIGKILL` across the
/// process group so no orphan grandchild processes remain.
async fn teardown_process_group(
    child: &mut Child,
    pgid: Option<u32>,
    grace: Duration,
) -> &'static str {
    let mut child_reaped = child.try_wait().ok().flatten().is_some();

    // 1. Wait up to `grace` for clean exit after session/cancel.
    if !grace.is_zero() {
        let deadline = tokio::time::Instant::now() + grace;
        if !child_reaped {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(Ok(_)) = tokio::time::timeout(rem, child.wait()).await {
                child_reaped = true;
            }
        }
        if child_reaped {
            while process_group_alive(pgid) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            if !process_group_alive(pgid) {
                return "graceful";
            }
        }
    } else if child_reaped && !process_group_alive(pgid) {
        return "graceful";
    }

    // 2. Escalate to SIGTERM on the entire process group.
    #[cfg(unix)]
    if let Some(pg) = pgid {
        let _ = signal_process_group(pg, rustix::process::Signal::TERM);
    }
    let sigterm_wait = grace.clamp(Duration::from_millis(80), Duration::from_secs(2));
    let term_deadline = tokio::time::Instant::now() + sigterm_wait;
    if !child_reaped {
        let rem = term_deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Ok(_)) = tokio::time::timeout(rem, child.wait()).await {
            child_reaped = true;
        }
    }
    if child_reaped {
        while process_group_alive(pgid) && tokio::time::Instant::now() < term_deadline {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        if !process_group_alive(pgid) {
            return "sigterm";
        }
    }

    // 3. Final escalation: SIGKILL on the entire process group + reap direct child.
    #[cfg(unix)]
    if let Some(pg) = pgid {
        let _ = signal_process_group(pg, rustix::process::Signal::KILL);
    }
    if !child_reaped {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let kill_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while process_group_alive(pgid) && tokio::time::Instant::now() < kill_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    "sigkill"
}

/// Terminates a detached process group recovered from `daemon.db` after a restart (`SIGTERM`,
/// grace wait, then `SIGKILL`).
async fn terminate_detached_process_group(pgid: Option<u32>, grace: Duration) {
    #[cfg(unix)]
    if let Some(pg) = pgid {
        let _ = signal_process_group(pg, rustix::process::Signal::TERM);
        let sigterm_wait = grace.clamp(Duration::from_millis(80), Duration::from_secs(2));
        let deadline = tokio::time::Instant::now() + sigterm_wait;
        while process_group_alive(Some(pg)) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        if process_group_alive(Some(pg)) {
            let _ = signal_process_group(pg, rustix::process::Signal::KILL);
            let kill_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            while process_group_alive(Some(pg)) && tokio::time::Instant::now() < kill_deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, grace);
    }
}

#[cfg(unix)]
fn signal_process_group(pgid: u32, signal: rustix::process::Signal) -> bool {
    let Some(raw) = i32::try_from(pgid).ok().filter(|&p| p > 0) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    rustix::process::kill_process_group(pid, signal).is_ok()
}

#[cfg(unix)]
fn process_group_alive(pgid: Option<u32>) -> bool {
    let Some(raw) = pgid.and_then(|g| i32::try_from(g).ok()).filter(|&p| p > 0) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    match rustix::process::test_kill_process_group(pid) {
        Ok(()) => true,
        Err(rustix::io::Errno::SRCH) => false,
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn process_group_alive(_pgid: Option<u32>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_run_command_wraps_official_binary_with_exact_cgroup_flags() {
        let bin = Path::new("/usr/local/bin/official-acp-agent");
        let args = vec!["acp".to_owned(), "--stdio".to_owned()];
        let cgroup = CgroupConfig {
            enabled: true,
            memory_max: Some("4G".to_owned()),
            cpu_quota: Some("200%".to_owned()),
            tasks_max: Some(256),
            slice: Some("agent-board-runs.slice".to_owned()),
        };

        let wrapped = build_launch_command(bin, &args, &cgroup, true);
        assert_eq!(wrapped.program, OsString::from("systemd-run"));
        let str_args: Vec<String> = wrapped
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            str_args,
            vec![
                "--user",
                "--scope",
                "--quiet",
                "-p",
                "MemoryMax=4G",
                "-p",
                "CPUQuota=200%",
                "-p",
                "TasksMax=256",
                "-p",
                "Slice=agent-board-runs.slice",
                "--",
                "/usr/local/bin/official-acp-agent",
                "acp",
                "--stdio",
            ]
        );

        // Disabled cgroup or non-Linux target launches the official binary directly.
        let direct_non_linux = build_launch_command(bin, &args, &cgroup, false);
        assert_eq!(
            direct_non_linux.program,
            OsString::from("/usr/local/bin/official-acp-agent")
        );
        assert_eq!(
            direct_non_linux.args,
            vec![OsString::from("acp"), OsString::from("--stdio")]
        );

        let disabled = CgroupConfig {
            enabled: false,
            ..cgroup
        };
        let direct_disabled = build_launch_command(bin, &args, &disabled, true);
        assert_eq!(direct_disabled, direct_non_linux);
    }

    #[test]
    fn normalizes_standard_acp_session_update_variants() {
        let (kind, summary, meta) = normalize_session_update(&json!({
            "sessionId": "s-1",
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "Inspecting Cargo.toml" }
            }
        }));
        assert_eq!(kind, RunEventKind::Phase);
        assert_eq!(summary, "Inspecting Cargo.toml");
        assert_eq!(
            meta.get("update_type").map(String::as_str),
            Some("agent_thought_chunk")
        );

        let (kind, summary, meta) = normalize_session_update(&json!({
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "cargo test",
                "kind": "execute",
                "status": "in_progress"
            }
        }));
        assert_eq!(kind, RunEventKind::Tool);
        assert_eq!(summary, "cargo test (in_progress)");
        assert_eq!(meta.get("tool_call_id").map(String::as_str), Some("tc-1"));
        assert_eq!(meta.get("tool_kind").map(String::as_str), Some("execute"));

        let (kind, summary, _) = normalize_session_update(&json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Implemented the parser." }
            }
        }));
        assert_eq!(kind, RunEventKind::Note);
        assert_eq!(summary, "Implemented the parser.");

        let (kind, summary, _) = normalize_session_update(&json!({
            "update": {
                "sessionUpdate": "error",
                "message": "rate limit exceeded"
            }
        }));
        assert_eq!(kind, RunEventKind::Error);
        assert_eq!(summary, "rate limit exceeded");
    }

    #[test]
    fn decide_permission_honors_allow_and_deny_policies_and_fallbacks() {
        let full_params = json!({
            "sessionId": "sess-1",
            "toolCall": {
                "toolCallId": "tc-9",
                "title": "Run cargo test",
                "kind": "execute"
            },
            "options": [
                { "optionId": "opt-once", "kind": "allow_once", "name": "Allow once" },
                { "optionId": "opt-always", "kind": "allow_always", "name": "Always allow" },
                { "optionId": "opt-reject-always", "kind": "reject_always", "name": "Always deny" },
                { "optionId": "opt-reject-once", "kind": "reject_once", "name": "Deny once" }
            ]
        });

        // Allow prefers allow_always over allow_once regardless of input order.
        let allow_pref = decide_permission(AcpPermissionPolicy::Allow, Some(&full_params));
        assert_eq!(allow_pref.tool, "Run cargo test");
        assert_eq!(allow_pref.tool_call_id.as_deref(), Some("tc-9"));
        assert_eq!(allow_pref.tool_kind.as_deref(), Some("execute"));
        assert_eq!(allow_pref.options_count, 4);
        assert_eq!(
            allow_pref.outcome,
            PermissionOutcome::Selected {
                option_id: "opt-always".to_owned(),
                option_kind: "allow_always",
            }
        );

        // Allow falls back to allow_once when allow_always is absent.
        let once_only = json!({
            "toolCall": { "name": "bash" },
            "options": [
                { "optionId": "opt-reject", "kind": "reject_once" },
                { "optionId": "opt-once", "kind": "allow_once" }
            ]
        });
        let allow_once = decide_permission(AcpPermissionPolicy::Allow, Some(&once_only));
        assert_eq!(allow_once.tool, "bash");
        assert_eq!(
            allow_once.outcome,
            PermissionOutcome::Selected {
                option_id: "opt-once".to_owned(),
                option_kind: "allow_once",
            }
        );

        // Allow returns Cancelled when no allow option is offered.
        let reject_only = json!({
            "toolCall": { "title": "Dangerous write" },
            "options": [
                { "optionId": "opt-reject-once", "kind": "reject_once" },
                { "optionId": "opt-reject-always", "kind": "reject_always" }
            ]
        });
        assert_eq!(
            decide_permission(AcpPermissionPolicy::Allow, Some(&reject_only)).outcome,
            PermissionOutcome::Cancelled
        );

        // Deny prefers reject_once over reject_always, falls back to reject_always, else Cancelled.
        let deny_pref = decide_permission(AcpPermissionPolicy::Deny, Some(&full_params));
        assert_eq!(
            deny_pref.outcome,
            PermissionOutcome::Selected {
                option_id: "opt-reject-once".to_owned(),
                option_kind: "reject_once",
            }
        );

        let reject_always_only = json!({
            "options": [
                { "optionId": "opt-allow", "kind": "allow_always" },
                { "option_id": "opt-ra", "kind": "reject_always" }
            ]
        });
        let deny_fallback = decide_permission(AcpPermissionPolicy::Deny, Some(&reject_always_only));
        assert_eq!(deny_fallback.tool, "unknown tool");
        assert_eq!(
            deny_fallback.outcome,
            PermissionOutcome::Selected {
                option_id: "opt-ra".to_owned(),
                option_kind: "reject_always",
            }
        );

        let allow_only = json!({
            "options": [{ "optionId": "opt-allow", "kind": "allow_once" }]
        });
        assert_eq!(
            decide_permission(AcpPermissionPolicy::Deny, Some(&allow_only)).outcome,
            PermissionOutcome::Cancelled
        );
        assert_eq!(
            decide_permission(AcpPermissionPolicy::Allow, None).outcome,
            PermissionOutcome::Cancelled
        );

        // Explicit JSON null or empty fields must NOT shadow snake_case/fallback keys.
        let null_shadow = json!({
            "toolCall": { "title": null, "name": "bash", "toolCallId": "", "tool_call_id": "tc-42" },
            "options": [
                { "optionId": null, "option_id": "opt-snake", "kind": "allow_always" }
            ]
        });
        let parsed = decide_permission(AcpPermissionPolicy::Allow, Some(&null_shadow));
        assert_eq!(parsed.tool, "bash");
        assert_eq!(parsed.tool_call_id.as_deref(), Some("tc-42"));
        assert_eq!(
            parsed.outcome,
            PermissionOutcome::Selected {
                option_id: "opt-snake".to_owned(),
                option_kind: "allow_always",
            }
        );
    }
}
