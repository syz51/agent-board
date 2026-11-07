//! `board watch`: the device daemon.
//!
//! It follows the server's event stream and, on this device:
//! - starts a coordinator thread for each autopilot board that lists this device in
//!   `home_devices` and has no live coordinator (claim the lease as the device, start a T3
//!   thread with `prompts/coordinator.md`, claim again as the thread's agent);
//! - renews the coordinator lease while the thread is live;
//! - wakes the coordinator when a task becomes ready, a blocker clears or an ask is
//!   answered, and wakes a worker whose ask was answered;
//! - never wakes a paused agent before its `paused_until`.
//!
//! Wake-up messages contain only board-generated text and the user's own answers.
//!
//! A started thread is recorded before it is registered or named coordinator, so a failed
//! step is retried with the same thread; a new thread is started only once the recorded one
//! is gone. A board whose coordinator cannot be started here is retried with a growing delay
//! until its repos, home devices, autopilot switch or status change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use board_proto::{
    Agent, AgentId, AgentRole, AgentSource, AgentState, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, DeviceName, ErrorCode, Event, EventType, RegisterAgent, Runtime, TaskId,
    Timestamp,
};
use chrono::{TimeDelta, Utc};
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::client::{Client, ServerError, error_code};
use crate::config::Home;
use crate::ops::Ctx;
use crate::state;
use crate::t3::{T3, T3Cli};

/// The coordinator's standing instructions.
pub const COORDINATOR_PROMPT: &str = include_str!("../../../prompts/coordinator.md");

/// Renew the 45-minute coordinator lease this often while the thread is live.
pub const RENEW_EVERY: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
const RECONCILE_EVERY: Duration = Duration::from_secs(60);
const FLUSH_EVERY: Duration = Duration::from_secs(3);
const STATE_NAME: &str = "watch";
/// Delay before retrying a coordinator that failed to start, doubled per failure up to the cap.
pub const START_RETRY_FIRST: TimeDelta = TimeDelta::minutes(1);
pub const START_RETRY_MAX: TimeDelta = TimeDelta::minutes(30);
/// Events whose handling failed on a transient error are retried for this long.
pub const RETRY_EVENTS_FOR: TimeDelta = TimeDelta::minutes(30);
const RETRY_EVENTS_MAX: usize = 500;
/// Event stream reconnect delay: doubled per failed or short-lived connection, up to the cap.
const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// A connection that stayed open this long resets the reconnect delay.
const STREAM_HEALTHY: Duration = Duration::from_secs(60);

/// What the daemon remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchState {
    /// The last event handled.
    pub last_seq: Option<i64>,
    /// Coordinator threads this device started, by board.
    #[serde(default)]
    pub coordinators: BTreeMap<BoardSlug, Coordinator>,
    /// Events whose handling failed on a transient error, retried on the reconcile tick.
    #[serde(default)]
    pub retry: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coordinator {
    pub thread: String,
    pub agent: AgentId,
    pub started_at: Timestamp,
    pub renewed_at: Timestamp,
    /// The `paused_until` the coordinator was last resumed after.
    pub resumed_after: Option<Timestamp>,
    /// Whether the thread's agent is registered; false right after the thread started.
    #[serde(default = "registered_default")]
    pub registered: bool,
}

/// Records written before this field existed were registered before they were saved.
fn registered_default() -> bool {
    true
}

/// The board settings that decide whether this device can start its coordinator.
#[derive(Debug, Clone, PartialEq)]
struct HostingConfig {
    repos: Vec<String>,
    autopilot: bool,
    status: BoardStatus,
    home_devices: Vec<DeviceName>,
}

impl HostingConfig {
    fn of(board: &Board) -> Self {
        Self {
            repos: board.repos.clone(),
            autopilot: board.autopilot,
            status: board.status,
            home_devices: board.home_devices.clone(),
        }
    }
}

/// A failed coordinator start, retried at `retry_at` unless the board's settings change.
#[derive(Debug, Clone)]
struct StartFailure {
    config: HostingConfig,
    delay: TimeDelta,
    retry_at: Timestamp,
}

pub struct Daemon<T: T3> {
    ctx: Ctx,
    t3: T,
    pub state: WatchState,
    boards: BTreeMap<BoardSlug, Board>,
    /// Wake-up lines waiting to be sent, by agent.
    pending: BTreeMap<AgentId, Vec<String>>,
    /// Boards whose coordinator failed to start here, by board.
    failures: BTreeMap<BoardSlug, StartFailure>,
}

impl<T: T3> Daemon<T> {
    /// A daemon acting as the device of `ctx` (whose client must carry no agent id).
    pub fn new(ctx: Ctx, t3: T) -> Self {
        let state = state::load_named(&ctx.home, STATE_NAME);
        Self {
            ctx,
            t3,
            state,
            boards: BTreeMap::new(),
            pending: BTreeMap::new(),
            failures: BTreeMap::new(),
        }
    }

    pub fn save(&self) -> Result<()> {
        state::save_named(&self.ctx.home, STATE_NAME, &self.state)
    }

    fn client(&self) -> &Client {
        &self.ctx.client
    }

    fn is_home(&self, board: &Board) -> bool {
        board.autopilot
            && board.status == BoardStatus::Active
            && board.home_devices.contains(&self.ctx.device)
    }

    /// Starts, renews or resumes the coordinators this device hosts.
    pub async fn reconcile(&mut self, now: Timestamp) -> Result<()> {
        let boards = self.client().boards().await?;
        self.boards = boards.iter().map(|b| (b.slug.clone(), b.clone())).collect();
        for board in &boards {
            if !self.is_home(board) {
                self.failures.remove(&board.slug);
                if self.state.coordinators.remove(&board.slug).is_some() {
                    self.release(&board.slug).await;
                }
                continue;
            }
            if let Err(err) = self.reconcile_board(board, now).await {
                tracing::warn!("coordinator of {}: {err:#}", board.slug);
            }
        }
        let boards = &self.boards;
        self.state
            .coordinators
            .retain(|slug, _| boards.contains_key(slug));
        self.failures.retain(|slug, _| boards.contains_key(slug));
        self.save()
    }

    async fn reconcile_board(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        // Our own coordinator whose thread is gone: replace it whatever its agent record says.
        let mut ours_gone = false;
        if let Some(local) = self.state.coordinators.get(slug).cloned() {
            // No coordinator named: the lease lapsed (the device slept) or the start did not
            // finish. The thread may still be ours to run.
            if board.coordinator.as_ref().is_none_or(|c| *c == local.agent) {
                let live = self
                    .t3
                    .thread_info(&local.thread)
                    .await?
                    .is_some_and(|t| !t.archived);
                if live {
                    return if board.coordinator.is_some() && local.registered {
                        self.keep_alive(board, local, now).await
                    } else {
                        self.adopt(board, local, now).await
                    };
                }
                ours_gone = board.coordinator.is_some();
            }
            // The board names someone else, or the thread is gone.
            self.state.coordinators.remove(slug);
        }
        if let Some(agent) = board.coordinator.as_ref().filter(|_| !ours_gone) {
            match self.client().agent_info(agent).await {
                Ok(info) if info.state != AgentState::Ended => return Ok(()),
                Ok(_) => {}
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        if let Some(failure) = self.failures.get(slug)
            && failure.config == HostingConfig::of(board)
            && now < failure.retry_at
        {
            return Ok(());
        }
        match self.start_coordinator(board, now).await {
            Ok(()) => {
                self.failures.remove(slug);
                Ok(())
            }
            Err(err) => {
                self.note_failure(board, now);
                Err(err)
            }
        }
    }

    /// Delays the next start on `board`: doubled after each failure, reset when its hosting
    /// settings change.
    fn note_failure(&mut self, board: &Board, now: Timestamp) {
        let config = HostingConfig::of(board);
        let delay = match self.failures.get(&board.slug) {
            Some(last) if last.config == config => (last.delay * 2).min(START_RETRY_MAX),
            _ => START_RETRY_FIRST,
        };
        self.failures.insert(
            board.slug.clone(),
            StartFailure {
                config,
                delay,
                retry_at: now + delay,
            },
        );
    }

    /// Renews the lease of a live coordinator and resumes it after a usage limit.
    async fn keep_alive(
        &mut self,
        board: &Board,
        mut local: Coordinator,
        now: Timestamp,
    ) -> Result<()> {
        if now - local.renewed_at >= RENEW_EVERY {
            match self.claim(&board.slug, Some(&local.agent)).await {
                Ok(updated) => {
                    self.boards.insert(board.slug.clone(), updated);
                    local.renewed_at = now;
                }
                // Another device took the lease over: the coordinator is no longer ours to run.
                Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
                    self.state.coordinators.remove(&board.slug);
                    return Ok(());
                }
                // Retried on the next reconcile; resuming below does not depend on it.
                Err(err) => tracing::warn!("renewing the coordinator of {}: {err:#}", board.slug),
            }
        }
        let info = self.client().agent_info(&local.agent).await?;
        if info.state == AgentState::Paused
            && let Some(until) = info.paused_until
            && until <= now
            && local.resumed_after != Some(until)
        {
            self.t3
                .send(
                    &local.thread,
                    &format!(
                        "board {}: the usage limit has reset. Run board_snapshot and continue your loop.",
                        board.slug
                    ),
                )
                .await?;
            local.resumed_after = Some(until);
        }
        self.state.coordinators.insert(board.slug.clone(), local);
        Ok(())
    }

    /// Names a live thread of ours the board's coordinator: registers its agent when that
    /// has not happened yet, then claims the lease with it. On a transient failure the record
    /// stays, so the next reconcile tries again with the same thread.
    async fn adopt(&mut self, board: &Board, mut local: Coordinator, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        if !local.registered {
            self.register_coordinator(board, &local).await?;
            local.registered = true;
            self.state.coordinators.insert(slug.clone(), local.clone());
        }
        match self.claim(slug, Some(&local.agent)).await {
            Ok(updated) => {
                self.boards.insert(slug.clone(), updated);
                local.renewed_at = now;
                tracing::info!("{} coordinates {slug}", local.agent);
                self.state.coordinators.insert(slug.clone(), local);
                Ok(())
            }
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
                tracing::warn!(
                    "another device took the coordinator lease of {slug}; thread {} is no longer its coordinator",
                    local.thread
                );
                self.state.coordinators.remove(slug);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    async fn register_coordinator(&self, board: &Board, local: &Coordinator) -> Result<()> {
        let slug = &board.slug;
        let (repo, project) = self.project_for(board).with_context(|| no_project(board))?;
        let runtime = match self.t3.thread_info(&local.thread).await {
            Ok(Some(info)) => info.runtime,
            _ => Runtime::Claude,
        };
        self.client()
            .with_agent(Some(local.agent.clone()))
            .register(&RegisterAgent {
                id: local.agent.clone(),
                runtime,
                device: self.ctx.device.clone(),
                role: AgentRole::Coordinator,
                title: Some(format!("{slug} coordinator")),
                repo: Some(repo),
                branch: None,
                cwd: Some(project.display().to_string()),
                native_ids: [("t3_thread".to_owned(), local.thread.clone())].into(),
                forked_from: None,
            })
            .await?;
        Ok(())
    }

    async fn claim(&self, slug: &BoardSlug, agent: Option<&AgentId>) -> Result<Board> {
        let client = self.client().with_agent(agent.cloned());
        client
            .claim_coordinator(
                slug,
                &ClaimCoordinator {
                    device: self.ctx.device.clone(),
                    agent: agent.cloned(),
                },
            )
            .await
    }

    /// Releases the coordinator lease, logging a failure: the lease expires on its own.
    async fn release(&mut self, slug: &BoardSlug) {
        match self.client().release_coordinator(slug).await {
            Ok(board) => {
                self.boards.insert(slug.clone(), board);
            }
            Err(err) => tracing::warn!("releasing the coordinator lease of {slug}: {err:#}"),
        }
    }

    /// The local T3 project folder for one of the board's repos.
    fn project_for(&self, board: &Board) -> Option<(String, PathBuf)> {
        board.repos.iter().find_map(|repo| {
            self.ctx
                .config
                .projects
                .get(repo)
                .map(|path| (repo.clone(), path.clone()))
        })
    }

    async fn start_coordinator(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        let slug = &board.slug;
        // Checked before claiming: a device that cannot host the thread leaves the lease alone.
        let Some((_, project)) = self.project_for(board) else {
            anyhow::bail!(no_project(board));
        };
        match self.claim(slug, None).await {
            Ok(updated) => {
                self.boards.insert(slug.clone(), updated);
            }
            // Another device holds the lease.
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => return Ok(()),
            Err(err) => return Err(err),
        }
        let title = format!("{slug} coordinator");
        let started = match self.unstarted_thread(&project, &title).await {
            Some(thread) => {
                tracing::warn!(
                    "coordinator of {slug}: briefing thread {thread}, which an earlier start created but never sent its message (the daemon stopped or t3 failed mid-start)"
                );
                self.t3
                    .send(&thread, &coordinator_message(slug))
                    .await
                    .map(|()| thread)
            }
            None => {
                self.t3
                    .start_thread(&project, &title, &coordinator_message(slug))
                    .await
            }
        };
        let thread = match started {
            Ok(thread) => thread,
            Err(err) => {
                self.release(slug).await;
                return Err(err);
            }
        };
        let agent = match format!("t3:{thread}").parse::<AgentId>() {
            Ok(agent) => agent,
            Err(err) => {
                self.release(slug).await;
                return Err(anyhow::Error::new(err).context(format!(
                    "t3 started thread {thread:?}, whose id is no agent id; archive it in T3"
                )));
            }
        };
        tracing::info!("started coordinator thread {thread} for {slug}");
        let local = Coordinator {
            thread,
            agent,
            started_at: now,
            renewed_at: now,
            resumed_after: None,
            registered: false,
        };
        // Recorded before anything else can fail, so a retry reuses this thread.
        self.state.coordinators.insert(slug.clone(), local.clone());
        self.save()?;
        self.adopt(board, local, now).await
    }

    /// The newest thread titled `title` that never ran: what a start leaves behind when it
    /// dies after T3 created the thread but before its message was sent. Reusing it keeps a
    /// retried start from adding a second thread. A failed listing falls back to a new thread.
    async fn unstarted_thread(&self, project: &Path, title: &str) -> Option<String> {
        match self.t3.unstarted_threads(project, title).await {
            Ok(threads) => threads.into_iter().next(),
            Err(err) => {
                tracing::warn!("listing the threads of {}: {err:#}", project.display());
                None
            }
        }
    }

    /// Handles one event: queues its wake-ups and advances `last_seq`. Returns true when the
    /// boards should be reconciled. On an error nothing is queued and `last_seq` stays.
    pub async fn on_event(&mut self, event: &Event, now: Timestamp) -> Result<bool> {
        let (reconcile, wakes) = self.examine(event, now).await?;
        for (agent, line) in wakes {
            let lines = self.pending.entry(agent).or_default();
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
        Ok(reconcile)
    }

    /// What one event calls for: a reconcile, and wake-up lines by agent. Only reads.
    async fn examine(
        &self,
        event: &Event,
        now: Timestamp,
    ) -> Result<(bool, Vec<(AgentId, String)>)> {
        let mut wakes = Vec::new();
        let Some(slug) = &event.board else {
            return Ok((false, wakes));
        };
        match event.event_type {
            EventType::BoardCreated => return Ok((true, wakes)),
            // This device's own lease claims and releases change nothing a reconcile would act on.
            EventType::BoardUpdated => return Ok((!self.is_own_claim(event), wakes)),
            EventType::TaskCreated | EventType::TaskReleased | EventType::TaskUpdated => {
                if let Some(task) = &event.task
                    && self.coordinator_here(slug).is_some()
                {
                    let detail = self.client().task(task).await?;
                    if detail.task.ready && detail.task.auto {
                        let line = format!("{task} is ready ({})", priority(&detail.task.priority));
                        wakes.extend(self.coordinator_wake(slug, &event.actor, line, now).await?);
                    }
                }
            }
            EventType::TaskDone => {
                if let Some(task) = &event.task
                    && self.coordinator_here(slug).is_some()
                {
                    let detail = self.client().task(task).await?;
                    let mut ready: Vec<TaskId> = Vec::new();
                    for blocked in &detail.blocks {
                        let t = self.client().task(blocked).await?.task;
                        if t.ready && t.auto {
                            ready.push(t.id);
                        }
                    }
                    if !ready.is_empty() {
                        let ids: Vec<&str> = ready.iter().map(TaskId::as_str).collect();
                        let line =
                            format!("{task} is done; its blocker cleared for {}", ids.join(", "));
                        wakes.extend(self.coordinator_wake(slug, &event.actor, line, now).await?);
                    }
                }
            }
            EventType::AskAnswered => wakes = self.on_answer(slug, event, now).await?,
            _ => {}
        }
        Ok((false, wakes))
    }

    /// A lease claim this device made (the server records the claiming device).
    fn is_own_claim(&self, event: &Event) -> bool {
        event.payload["device"].as_str() == Some(self.ctx.device.as_str())
            && event.payload["fields"] == serde_json::json!(["coordinator"])
    }

    async fn on_answer(
        &self,
        slug: &BoardSlug,
        event: &Event,
        now: Timestamp,
    ) -> Result<Vec<(AgentId, String)>> {
        let mut wakes = Vec::new();
        let Some(id) = event.payload["ask"]
            .as_i64()
            .or_else(|| event.payload["id"].as_i64())
        else {
            tracing::warn!("event #{}: ask_answered without an ask id", event.seq);
            return Ok(wakes);
        };
        let asks = self.client().asks(Some("answered"), Some(slug)).await?;
        let Some(ask) = asks.into_iter().find(|a| a.id == id) else {
            return Ok(wakes);
        };
        let about = ask
            .task
            .as_ref()
            .map(|t| format!(" about {t}"))
            .unwrap_or_default();
        let answer = serde_json::to_string(ask.answer.as_deref().unwrap_or_default())?
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        let line = format!("the user answered ask #{id}{about}: {answer}");
        wakes.extend(
            self.wake(&ask.agent, &event.actor, line.clone(), now)
                .await?,
        );
        if self.coordinator_here(slug).is_some()
            && let Some(coordinator) = self.boards.get(slug).and_then(|b| b.coordinator.clone())
            && coordinator != ask.agent
        {
            wakes.extend(self.wake(&coordinator, &event.actor, line, now).await?);
        }
        Ok(wakes)
    }

    /// The coordinator of an autopilot board, when there is one.
    fn coordinator_here(&self, slug: &BoardSlug) -> Option<&AgentId> {
        self.boards
            .get(slug)
            .filter(|b| b.autopilot && b.status == BoardStatus::Active)
            .and_then(|b| b.coordinator.as_ref())
    }

    async fn coordinator_wake(
        &self,
        slug: &BoardSlug,
        actor: &str,
        line: String,
        now: Timestamp,
    ) -> Result<Option<(AgentId, String)>> {
        match self.coordinator_here(slug) {
            Some(agent) => {
                self.wake(agent, actor, format!("board {slug}: {line}"), now)
                    .await
            }
            None => Ok(None),
        }
    }

    /// `line` for `agent` when it is a T3 thread on this device that did not cause the event
    /// and is not paused.
    async fn wake(
        &self,
        agent: &AgentId,
        actor: &str,
        line: String,
        now: Timestamp,
    ) -> Result<Option<(AgentId, String)>> {
        if agent.as_str() == actor || agent.source() != AgentSource::T3 {
            return Ok(None);
        }
        let info = match self.client().agent_info(agent).await {
            Ok(info) => info,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(should_wake(&info, &self.ctx.device, now).then(|| (agent.clone(), line)))
    }

    /// Sends the queued wake-ups, one message per agent.
    pub async fn flush(&mut self) {
        for (agent, lines) in std::mem::take(&mut self.pending) {
            let message = format!("{}\nRun board_snapshot and continue.", lines.join("\n"));
            if let Err(err) = self.t3.send(agent.native(), &message).await {
                tracing::warn!("waking {agent}: {err:#}");
            }
        }
    }

    /// Handles every event after `last_seq` through the paged events endpoint.
    pub async fn catch_up(&mut self, now: Timestamp) -> Result<bool> {
        let mut reconcile = false;
        loop {
            let since = self.state.last_seq;
            let page = self.client().events(since, None, Some(200)).await?;
            for event in &page.events {
                reconcile |= self.handle(event, now).await;
            }
            if page.events.is_empty() || Some(page.next_since) == since {
                break;
            }
            self.state.last_seq = Some(page.next_since);
        }
        Ok(reconcile)
    }

    /// Handles an event from the feed. One that fails on a transient error (the server
    /// restarting, the network down) is kept for [`Self::retry_failed`]; either way the feed
    /// moves past it, so one bad event cannot stall the daemon.
    async fn handle(&mut self, event: &Event, now: Timestamp) -> bool {
        match self.on_event(event, now).await {
            Ok(reconcile) => reconcile,
            Err(err) => {
                if is_transient(&err) {
                    tracing::warn!("event #{}: {err:#}; retrying later", event.seq);
                    if !self.state.retry.iter().any(|e| e.seq == event.seq) {
                        self.state.retry.push(event.clone());
                    }
                    if self.state.retry.len() > RETRY_EVENTS_MAX {
                        let dropped = self.state.retry.remove(0);
                        tracing::warn!("event #{}: too many to retry; dropped", dropped.seq);
                    }
                } else {
                    tracing::warn!("event #{}: {err:#}", event.seq);
                }
                self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
                false
            }
        }
    }

    /// Handles again the events that failed on a transient error, dropping those older than
    /// [`RETRY_EVENTS_FOR`] or failing for good. Returns true when the boards should be
    /// reconciled.
    pub async fn retry_failed(&mut self, now: Timestamp) -> bool {
        let mut reconcile = false;
        for event in std::mem::take(&mut self.state.retry) {
            if now - event.at > RETRY_EVENTS_FOR {
                tracing::warn!("event #{}: still failing; dropped", event.seq);
                continue;
            }
            match self.on_event(&event, now).await {
                Ok(again) => reconcile |= again,
                Err(err) if is_transient(&err) => self.state.retry.push(event),
                Err(err) => tracing::warn!("event #{}: {err:#}; dropped", event.seq),
            }
        }
        reconcile
    }
}

/// Is this a failure that may pass on its own: no answer, or a server-side error?
fn is_transient(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<ServerError>() {
        Some(server) => !server.status.is_client_error(),
        None => true,
    }
}

fn no_project(board: &Board) -> String {
    format!(
        "no T3 project for {} on this device: add it under [projects] in config.toml",
        board.repos.join(", ")
    )
}

/// May the daemon on `device` wake this agent now?
pub fn should_wake(agent: &Agent, device: &board_proto::DeviceName, now: Timestamp) -> bool {
    if &agent.device != device || agent.id.source() != AgentSource::T3 {
        return false;
    }
    match agent.state {
        // Never before the usage limit resets; without a reset time, leave it to the user.
        AgentState::Paused => agent.paused_until.is_some_and(|until| until <= now),
        _ => true,
    }
}

fn priority(p: &board_proto::Priority) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The first message of a coordinator thread.
pub fn coordinator_message(slug: &BoardSlug) -> String {
    format!(
        "{COORDINATOR_PROMPT}\nYou are the coordinator of board {slug}. Start now: call board_snapshot."
    )
}

/// Runs the daemon until interrupted or stopped (or one pass with `once`).
pub async fn run(home: Home, once: bool) -> Result<()> {
    // Listen first, so a stop during start-up is handled once the loop runs.
    let mut shutdown = Shutdown::listen()?;
    let ctx = Ctx::load(home, None, None)?;
    let t3 = T3Cli::locate(ctx.config.t3_bin.as_deref());
    let mut daemon = Daemon::new(ctx, t3);
    if daemon.state.last_seq.is_none() {
        // Start from now: history is not news.
        let health = daemon.client().health().await?;
        daemon.state.last_seq = health["seq"].as_i64();
    }
    daemon.reconcile(Utc::now()).await?;
    if once {
        let mut reconcile = daemon.retry_failed(Utc::now()).await;
        reconcile |= daemon.catch_up(Utc::now()).await?;
        if reconcile {
            daemon.reconcile(Utc::now()).await?;
        }
        daemon.flush().await;
        return daemon.save();
    }
    daemon.follow(&mut shutdown).await
}

/// Ctrl-C, or SIGTERM from launchd or systemd stopping the service.
pub struct Shutdown {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Shutdown {
    pub fn listen() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
                terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// Resolves when the daemon is asked to stop.
    pub async fn wait(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
        #[cfg(not(unix))]
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Sleeps `delay` unless asked to stop (true), then doubles it up to the cap.
async fn wait_to_reconnect(shutdown: &mut Shutdown, delay: &mut Duration) -> bool {
    let stop = tokio::select! {
        () = tokio::time::sleep(*delay) => false,
        () = shutdown.wait() => true,
    };
    *delay = (*delay * 2).min(RECONNECT_MAX);
    stop
}

impl<T: T3> Daemon<T> {
    async fn follow(&mut self, shutdown: &mut Shutdown) -> Result<()> {
        let mut delay = RECONNECT_FIRST;
        let mut reconcile_tick = tokio::time::interval(RECONCILE_EVERY);
        let mut flush_tick = tokio::time::interval(FLUSH_EVERY);
        let mut reconcile_needed = false;
        loop {
            let client = self.client().clone();
            let connected = tokio::select! {
                connected = client.event_stream(self.state.last_seq) => connected,
                () = shutdown.wait() => return self.stop().await,
            };
            let resp = match connected {
                Ok(resp) => resp,
                Err(err) => {
                    tracing::warn!("event stream: {err:#}; retrying in {}s", delay.as_secs());
                    if wait_to_reconnect(shutdown, &mut delay).await {
                        return self.stop().await;
                    }
                    continue;
                }
            };
            let opened = tokio::time::Instant::now();
            let mut stream = resp.bytes_stream().eventsource();
            loop {
                tokio::select! {
                    item = stream.next() => match item {
                        Some(Ok(message)) => {
                            delay = RECONNECT_FIRST;
                            match serde_json::from_str::<Event>(&message.data) {
                                Ok(event) => reconcile_needed |= self.handle(&event, Utc::now()).await,
                                Err(err) => tracing::warn!("unreadable event: {err}"),
                            }
                        }
                        Some(Err(err)) => {
                            tracing::warn!("event stream: {err}");
                            break;
                        }
                        None => break,
                    },
                    _ = flush_tick.tick() => {
                        if reconcile_needed {
                            reconcile_needed = false;
                            self.reconcile_logged().await;
                        }
                        self.flush().await;
                        self.save()?;
                    }
                    _ = reconcile_tick.tick() => {
                        reconcile_needed |= self.retry_failed(Utc::now()).await;
                        self.reconcile_logged().await;
                    }
                    () = shutdown.wait() => return self.stop().await,
                }
            }
            if opened.elapsed() >= STREAM_HEALTHY {
                delay = RECONNECT_FIRST;
            }
            tracing::info!("event stream ended; reconnecting in {}s", delay.as_secs());
            if wait_to_reconnect(shutdown, &mut delay).await {
                return self.stop().await;
            }
        }
    }

    /// Sends what is queued and saves the state before exiting.
    async fn stop(&mut self) -> Result<()> {
        tracing::info!("stopping");
        self.flush().await;
        self.save()
    }

    async fn reconcile_logged(&mut self) {
        if let Err(err) = self.reconcile(Utc::now()).await {
            tracing::warn!("reconcile: {err:#}");
        }
    }
}
