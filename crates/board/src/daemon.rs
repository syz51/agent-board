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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use board_proto::{
    Agent, AgentId, AgentRole, AgentSource, AgentState, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, ErrorCode, Event, EventType, RegisterAgent, Runtime, TaskId, Timestamp,
};
use chrono::Utc;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::client::{Client, error_code};
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

/// What the daemon remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchState {
    /// The last event handled.
    pub last_seq: Option<i64>,
    /// Coordinator threads this device started, by board.
    #[serde(default)]
    pub coordinators: BTreeMap<BoardSlug, Coordinator>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coordinator {
    pub thread: String,
    pub agent: AgentId,
    pub started_at: Timestamp,
    pub renewed_at: Timestamp,
    /// The `paused_until` the coordinator was last resumed after.
    pub resumed_after: Option<Timestamp>,
}

pub struct Daemon<T: T3> {
    ctx: Ctx,
    t3: T,
    pub state: WatchState,
    boards: BTreeMap<BoardSlug, Board>,
    /// Wake-up lines waiting to be sent, by agent.
    pending: BTreeMap<AgentId, Vec<String>>,
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
                if self.state.coordinators.remove(&board.slug).is_some()
                    && let Err(err) = self.client().release_coordinator(&board.slug).await
                {
                    tracing::debug!("releasing the coordinator of {}: {err:#}", board.slug);
                }
                continue;
            }
            if let Err(err) = self.reconcile_board(board, now).await {
                tracing::warn!("coordinator of {}: {err:#}", board.slug);
            }
        }
        let known: Vec<BoardSlug> = self.state.coordinators.keys().cloned().collect();
        for slug in known {
            if !self.boards.contains_key(&slug) {
                self.state.coordinators.remove(&slug);
            }
        }
        self.save()
    }

    async fn reconcile_board(&mut self, board: &Board, now: Timestamp) -> Result<()> {
        // Our own coordinator whose thread is gone: replace it whatever its agent record says.
        let mut ours_gone = false;
        if let Some(local) = self.state.coordinators.get(&board.slug).cloned() {
            if board.coordinator.as_ref() == Some(&local.agent) {
                let live = self
                    .t3
                    .thread_info(&local.thread)
                    .await?
                    .is_some_and(|t| !t.archived);
                if live {
                    return self.keep_alive(board, local, now).await;
                }
                ours_gone = true;
            }
            // The board names someone else, or the thread is gone.
            self.state.coordinators.remove(&board.slug);
        }
        if let Some(agent) = board.coordinator.as_ref().filter(|_| !ours_gone) {
            match self.client().agent_info(agent).await {
                Ok(info) if info.state != AgentState::Ended => return Ok(()),
                Ok(_) => {}
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        self.start_coordinator(board, now).await
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
                Ok(_) => local.renewed_at = now,
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
        match self.claim(slug, None).await {
            Ok(_) => {}
            // Another device holds the lease.
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => return Ok(()),
            Err(err) => return Err(err),
        }
        let Some((repo, project)) = self.project_for(board) else {
            let _ = self.client().release_coordinator(slug).await;
            anyhow::bail!(
                "no T3 project for {} on this device: add it under [projects] in config.toml",
                board.repos.join(", ")
            );
        };
        let started = self
            .t3
            .start_thread(
                &project,
                &format!("{slug} coordinator"),
                &coordinator_message(slug),
            )
            .await;
        let thread = match started {
            Ok(thread) => thread,
            Err(err) => {
                let _ = self.client().release_coordinator(slug).await;
                return Err(err);
            }
        };
        let agent: AgentId = format!("t3:{thread}")
            .parse()
            .with_context(|| format!("thread id {thread:?} from t3"))?;
        let runtime = match self.t3.thread_info(&thread).await {
            Ok(Some(info)) => info.runtime,
            _ => Runtime::Claude,
        };
        self.client()
            .with_agent(Some(agent.clone()))
            .register(&RegisterAgent {
                id: agent.clone(),
                runtime,
                device: self.ctx.device.clone(),
                role: AgentRole::Coordinator,
                title: Some(format!("{slug} coordinator")),
                repo: Some(repo),
                branch: None,
                cwd: Some(project.display().to_string()),
                native_ids: [("t3_thread".to_owned(), thread.clone())].into(),
                forked_from: None,
            })
            .await?;
        self.claim(slug, Some(&agent)).await?;
        tracing::info!("started coordinator {agent} for {slug}");
        self.state.coordinators.insert(
            slug.clone(),
            Coordinator {
                thread,
                agent,
                started_at: now,
                renewed_at: now,
                resumed_after: None,
            },
        );
        Ok(())
    }

    /// Queues wake-ups for one event. Returns true when the boards should be reconciled.
    pub async fn on_event(&mut self, event: &Event, now: Timestamp) -> Result<bool> {
        self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
        let Some(slug) = &event.board else {
            return Ok(false);
        };
        match event.event_type {
            EventType::BoardCreated | EventType::BoardUpdated => return Ok(true),
            EventType::TaskCreated | EventType::TaskReleased | EventType::TaskUpdated => {
                if let Some(task) = &event.task
                    && self.coordinator_here(slug).is_some()
                {
                    let detail = self.client().task(task).await?;
                    if detail.task.ready && detail.task.auto {
                        let line = format!("{task} is ready ({})", priority(&detail.task.priority));
                        self.wake_coordinator(slug, &event.actor, line, now).await?;
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
                        self.wake_coordinator(slug, &event.actor, line, now).await?;
                    }
                }
            }
            EventType::AskAnswered => self.on_answer(slug, event, now).await?,
            _ => {}
        }
        Ok(false)
    }

    async fn on_answer(&mut self, slug: &BoardSlug, event: &Event, now: Timestamp) -> Result<()> {
        let id = event.payload["ask"]
            .as_i64()
            .or_else(|| event.payload["id"].as_i64())
            .context("ask_answered event without an ask id")?;
        let asks = self.client().asks(Some("answered"), Some(slug)).await?;
        let Some(ask) = asks.into_iter().find(|a| a.id == id) else {
            return Ok(());
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
        self.wake(&ask.agent, &event.actor, line.clone(), now)
            .await?;
        if self.coordinator_here(slug).is_some()
            && let Some(coordinator) = self.boards.get(slug).and_then(|b| b.coordinator.clone())
            && coordinator != ask.agent
        {
            self.wake(&coordinator, &event.actor, line, now).await?;
        }
        Ok(())
    }

    /// The coordinator of an autopilot board, when there is one.
    fn coordinator_here(&self, slug: &BoardSlug) -> Option<&AgentId> {
        self.boards
            .get(slug)
            .filter(|b| b.autopilot && b.status == BoardStatus::Active)
            .and_then(|b| b.coordinator.as_ref())
    }

    async fn wake_coordinator(
        &mut self,
        slug: &BoardSlug,
        actor: &str,
        line: String,
        now: Timestamp,
    ) -> Result<()> {
        if let Some(agent) = self.coordinator_here(slug).cloned() {
            self.wake(&agent, actor, format!("board {slug}: {line}"), now)
                .await?;
        }
        Ok(())
    }

    /// Queues `line` for `agent` when it is a T3 thread on this device that did not cause
    /// the event and is not paused.
    async fn wake(
        &mut self,
        agent: &AgentId,
        actor: &str,
        line: String,
        now: Timestamp,
    ) -> Result<()> {
        if agent.as_str() == actor || agent.source() != AgentSource::T3 {
            return Ok(());
        }
        let info = match self.client().agent_info(agent).await {
            Ok(info) => info,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => return Ok(()),
            Err(err) => return Err(err),
        };
        if should_wake(&info, &self.ctx.device, now) {
            let lines = self.pending.entry(agent.clone()).or_default();
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        Ok(())
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

    async fn handle(&mut self, event: &Event, now: Timestamp) -> bool {
        match self.on_event(event, now).await {
            Ok(reconcile) => reconcile,
            Err(err) => {
                tracing::warn!("event #{}: {err:#}", event.seq);
                false
            }
        }
    }
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

/// Runs the daemon until interrupted (or one pass with `once`).
pub async fn run(home: Home, once: bool) -> Result<()> {
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
        if daemon.catch_up(Utc::now()).await? {
            daemon.reconcile(Utc::now()).await?;
        }
        daemon.flush().await;
        return daemon.save();
    }
    daemon.follow().await
}

impl<T: T3> Daemon<T> {
    async fn follow(&mut self) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        let mut reconcile_tick = tokio::time::interval(RECONCILE_EVERY);
        let mut flush_tick = tokio::time::interval(FLUSH_EVERY);
        let mut reconcile_needed = false;
        loop {
            let stream = match self.client().event_stream(self.state.last_seq).await {
                Ok(resp) => {
                    backoff = Duration::from_secs(1);
                    Some(eventsource_stream::Eventsource::eventsource(
                        resp.bytes_stream(),
                    ))
                }
                Err(err) => {
                    tracing::warn!("event stream: {err:#}; retrying in {}s", backoff.as_secs());
                    None
                }
            };
            let Some(mut stream) = stream else {
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = tokio::signal::ctrl_c() => return self.save(),
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            };
            loop {
                tokio::select! {
                    item = stream.next() => match item {
                        Some(Ok(message)) => match serde_json::from_str::<Event>(&message.data) {
                            Ok(event) => reconcile_needed |= self.handle(&event, Utc::now()).await,
                            Err(err) => tracing::warn!("unreadable event: {err}"),
                        },
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
                    _ = reconcile_tick.tick() => self.reconcile_logged().await,
                    _ = tokio::signal::ctrl_c() => {
                        self.flush().await;
                        return self.save();
                    }
                }
            }
        }
    }

    async fn reconcile_logged(&mut self) {
        if let Err(err) = self.reconcile(Utc::now()).await {
            tracing::warn!("reconcile: {err:#}");
        }
    }
}
