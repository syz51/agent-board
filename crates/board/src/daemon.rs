//! `board watch`: the device daemon.
//!
//! It follows the server's event stream and, on this device:
//! - starts a coordinator thread for each autopilot board that lists this device in
//!   `home_devices` and has no live coordinator (claim the lease as the device, start a T3
//!   thread with `prompts/coordinator.md`, plus `prompts/triage.md` on the device's feedback
//!   board, claim again as the thread's agent);
//! - renews the coordinator lease while the thread is live;
//! - wakes the coordinator when a task becomes ready or a blocker clears, and the asker
//!   (worker or coordinator) when its ask is answered, or the coordinator when the asker
//!   cannot be woken;
//! - never wakes a paused agent before its `paused_until`.
//!
//! Wake-ups are queued as what they are about (a task, an ask), not as text. Each flush
//! checks them again before sending: a task no longer ready and auto drops its line. Lines
//! for a running agent are held until its `agent_state_changed` event says it stopped, at
//! most [`WAKE_HOLD_MAX`]; a paused agent's are kept until its reset. A failed send backs
//! off, a gone thread drops its lines, and a send that timed out is not repeated.
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
    ClaimCoordinator, DeviceName, ErrorCode, Event, EventType, Priority, RegisterAgent, Runtime,
    TaskId, Timestamp,
};
use chrono::{TimeDelta, Utc};
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::client::{Client, ServerError, error_code};
use crate::config::Home;
use crate::ops::Ctx;
use crate::state;
use crate::t3::{T3, T3Cli, TimedOut};

/// The coordinator's standing instructions.
pub const COORDINATOR_PROMPT: &str = include_str!("../../../prompts/coordinator.md");
/// The feedback board coordinator's triage instructions, after [`COORDINATOR_PROMPT`].
pub const TRIAGE_PROMPT: &str = include_str!("../../../prompts/triage.md");

/// Wake-ups for an agent shown running are held at most this long, then sent anyway: T3
/// gives a busy thread the message as its next turn, and a thread left marked running (a
/// turn ended on an API error, an interrupt, a Codex error) must still be woken.
pub const WAKE_HOLD_MAX: TimeDelta = TimeDelta::minutes(3);
/// A wake-up not delivered this long after it was queued, or after the reset its paused
/// agent waits for, is dropped.
pub const WAKE_TTL: TimeDelta = TimeDelta::hours(1);
/// Delay before sending again after a failed send, doubled per failure up to the cap.
pub const WAKE_RETRY_FIRST: TimeDelta = TimeDelta::seconds(30);
pub const WAKE_RETRY_MAX: TimeDelta = TimeDelta::minutes(10);
/// How long one wake-up send may take; one that runs out is not sent again, since T3 may
/// have delivered it.
const WAKE_SEND_TIMEOUT: Duration = Duration::from_secs(20);

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
    /// Boards whose coordinator start began at this time and has not recorded its thread.
    /// Saved before `t3 thread start` runs, so a start the daemon did not live to finish is
    /// found again: the next start adopts the thread it created instead of adding another.
    #[serde(default)]
    pub pending_starts: BTreeMap<BoardSlug, Timestamp>,
    /// Wake-ups not sent yet, by agent: held while the agent runs or is paused, or kept
    /// after a failed send. Saved so a restart does not lose them; an entry this version
    /// cannot read is skipped rather than resetting the rest of the state.
    #[serde(default, deserialize_with = "lenient_queues")]
    pub wakes: BTreeMap<AgentId, WakeQueue>,
}

/// One agent's wake-ups not sent yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WakeQueue {
    #[serde(default, deserialize_with = "lenient_wakes")]
    pub wakes: Vec<Wake>,
    /// Set after a failed send: nothing is sent before `retry_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<Backoff>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backoff {
    pub delay_secs: i64,
    pub retry_at: Timestamp,
}

/// A queued wake-up: what it is about, checked again and rendered when it is sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wake {
    pub board: BoardSlug,
    pub about: WakeAbout,
    pub queued_at: Timestamp,
    /// An answer whose asker is paused, already passed on to the coordinator.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeAbout {
    /// For the coordinator: the task became ready.
    Ready { task: TaskId },
    /// For the coordinator: `done` cleared the last blocker of `ready`.
    Cleared { done: TaskId, ready: Vec<TaskId> },
    /// For the coordinator: another agent filed `task` as a proposal to review.
    Proposal { task: TaskId },
    /// The user answered `asker`'s ask: for the asker, or for the coordinator when the
    /// asker cannot be woken.
    Answered { ask: i64, asker: AgentId },
}

/// Each agent's queue read on its own, so one this version cannot read (a newer kind, a
/// damaged entry) is skipped instead of failing the whole state file.
fn lenient_queues<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<AgentId, WakeQueue>, D::Error> {
    let serde_json::Value::Object(queues) = serde_json::Value::deserialize(d)? else {
        return Ok(BTreeMap::new());
    };
    Ok(queues
        .into_iter()
        .filter_map(
            |(agent, queue)| match (agent.parse::<AgentId>(), serde_json::from_value(queue)) {
                (Ok(id), Ok(queue @ WakeQueue { .. })) => {
                    (!queue.wakes.is_empty()).then_some((id, queue))
                }
                _ => {
                    tracing::warn!(
                        "watch state: skipped the wake-ups for {agent}, which this version cannot read"
                    );
                    None
                }
            },
        )
        .collect())
}

/// Wake-ups read one by one; those this version cannot read are skipped.
fn lenient_wakes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Wake>, D::Error> {
    let serde_json::Value::Array(wakes) = serde_json::Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(wakes
        .into_iter()
        .filter_map(|wake| match serde_json::from_value(wake) {
            Ok(wake) => Some(wake),
            Err(err) => {
                tracing::warn!("watch state: skipped a wake-up this version cannot read: {err}");
                None
            }
        })
        .collect())
}

/// What the daemon knows of an agent with queued wake-ups: read from the server once, then
/// kept current by the agent's `agent_state_changed` events, so a hold needs no polling.
#[derive(Debug, Clone)]
struct Seen {
    agent: Agent,
    /// When `agent.state` was last read or changed.
    since: Timestamp,
    /// The agent stopped running since it was first read: its held wake-ups are due.
    idled: bool,
}

/// Whether an agent's wake-ups can be sent now.
enum Reach {
    Now,
    /// Running: held until it goes idle or for [`WAKE_HOLD_MAX`].
    Held,
    /// Paused until a usage-limit reset still to come.
    Paused,
    /// Not a T3 thread on this device, ended, or paused with no reset time.
    Never,
}

impl Reach {
    fn of(seen: &Seen, device: &DeviceName, now: Timestamp) -> Self {
        let agent = &seen.agent;
        if &agent.device != device || agent.id.source() != AgentSource::T3 {
            return Self::Never;
        }
        match agent.state {
            AgentState::Ended => Self::Never,
            AgentState::Paused => match agent.paused_until {
                Some(until) if until > now => Self::Paused,
                Some(_) => Self::Now,
                // Without a reset time, resuming is left to the user.
                None => Self::Never,
            },
            AgentState::Running if !seen.idled && now - seen.since < WAKE_HOLD_MAX => Self::Held,
            AgentState::Running | AgentState::Idle => Self::Now,
        }
    }
}

/// A wake-up message ready to send, with the wake-ups it carries.
struct Outgoing {
    agent: AgentId,
    message: String,
    wakes: Vec<Wake>,
    /// The queue's back-off before this send, which a failure doubles.
    backoff: Option<Backoff>,
}

/// How a wake-up send went.
enum Sent {
    Delivered,
    /// It ran out of time: T3 may have delivered it, so it is not sent again.
    MaybeDelivered(anyhow::Error),
    /// T3 no longer has the thread, or it is archived.
    Gone(anyhow::Error),
    Failed(anyhow::Error),
}

/// The next delay of a doubling back-off: `first`, then twice the `last`, up to `max`.
fn next_delay(last: Option<TimeDelta>, first: TimeDelta, max: TimeDelta) -> TimeDelta {
    last.map_or(first, |last| (last * 2).min(max))
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

/// A request from `board board adopt`: the next time the board needs a coordinator, this
/// device's daemon makes `thread`, an existing thread, its coordinator instead of starting
/// one. Kept in its own state file, `adopt-<board>.json`, so the CLI never writes the
/// daemon's `watch.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adoption {
    pub thread: String,
    pub requested_at: Timestamp,
}

/// An adoption not acted on for this long is dropped: by then a new thread is what the
/// user expects. It outlasts a coordinator lease, which an adoption may wait out.
pub const ADOPTION_TTL: TimeDelta = TimeDelta::hours(1);

fn adoption_name(slug: &BoardSlug) -> String {
    format!("adopt-{slug}")
}

/// The adoption waiting for `slug` on this device, if any.
pub fn load_adoption(home: &Home, slug: &BoardSlug) -> Option<Adoption> {
    state::load_named::<Option<Adoption>>(home, &adoption_name(slug))
}

pub fn save_adoption(home: &Home, slug: &BoardSlug, adoption: &Adoption) -> Result<()> {
    state::save_named(home, &adoption_name(slug), &Some(adoption))
}

pub fn remove_adoption(home: &Home, slug: &BoardSlug) -> Result<()> {
    state::remove_named(home, &adoption_name(slug))
}

/// Removes `adoption` unless a newer request replaced it in the meantime.
fn drop_adoption(home: &Home, slug: &BoardSlug, adoption: &Adoption) -> Result<()> {
    if load_adoption(home, slug).as_ref() == Some(adoption) {
        remove_adoption(home, slug)?;
    }
    Ok(())
}

/// The coordinators this device's daemon runs, as it last saved them.
pub fn load_watch_state(home: &Home) -> WatchState {
    state::load_named(home, STATE_NAME)
}

/// The agent id of a T3 thread.
pub fn thread_agent(thread: &str) -> Result<AgentId> {
    format!("t3:{thread}")
        .parse::<AgentId>()
        .with_context(|| format!("thread id {thread:?} makes no agent id"))
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
    /// Boards whose coordinator failed to start here, by board.
    failures: BTreeMap<BoardSlug, StartFailure>,
    /// Agents with queued wake-ups, as last read or reported.
    agents: BTreeMap<AgentId, Seen>,
}

impl<T: T3> Daemon<T> {
    /// A daemon acting as the device of `ctx` (whose client must carry no agent id).
    pub fn new(ctx: Ctx, t3: T) -> Self {
        let state = load_watch_state(&ctx.home);
        Self {
            ctx,
            t3,
            state,
            boards: BTreeMap::new(),
            failures: BTreeMap::new(),
            agents: BTreeMap::new(),
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
                self.state.pending_starts.remove(&board.slug);
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
        self.state
            .pending_starts
            .retain(|slug, _| boards.contains_key(slug));
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
        if ours_gone {
            // A renewal keeps the coordinator named, so the gone thread would stay named
            // until its replacement claims; releasing first means no one is named meanwhile.
            self.release(slug).await;
        } else if let Some(agent) = &board.coordinator {
            match self.client().agent_info(agent).await {
                Ok(info) if info.state != AgentState::Ended => return Ok(()),
                Ok(_) => {}
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        let adoption = match load_adoption(&self.ctx.home, slug) {
            Some(adoption) if now - adoption.requested_at >= ADOPTION_TTL => {
                tracing::warn!(
                    "coordinator of {slug}: dropped the adoption of thread {}, requested {} and never acted on",
                    adoption.thread,
                    adoption.requested_at
                );
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                None
            }
            adoption => adoption,
        };
        // An adoption requested after the last failed start is new intent: tried at once.
        if let Some(failure) = self.failures.get(slug)
            && failure.config == HostingConfig::of(board)
            && now < failure.retry_at
            && adoption
                .as_ref()
                .is_none_or(|a| a.requested_at <= failure.retry_at - failure.delay)
        {
            return Ok(());
        }
        let started = match adoption {
            Some(adoption) => self.adopt_thread(board, adoption, now).await,
            None => self.start_coordinator(board, now).await,
        };
        match started {
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
        let last = self
            .failures
            .get(&board.slug)
            .filter(|last| last.config == config)
            .map(|last| last.delay);
        let delay = next_delay(last, START_RETRY_FIRST, START_RETRY_MAX);
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
                title: Some(coordinator_title(slug)),
                repo,
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

    /// The local T3 project folder for one of the board's repos, with that repo; else the
    /// folder `[board_projects]` names for the board, without a repo.
    fn project_for(&self, board: &Board) -> Option<(Option<String>, PathBuf)> {
        let config = &self.ctx.config;
        board
            .repos
            .iter()
            .find_map(|repo| {
                config
                    .projects
                    .get(repo)
                    .map(|path| (Some(repo.clone()), path.clone()))
            })
            .or_else(|| {
                config
                    .board_projects
                    .get(&board.slug)
                    .map(|path| (None, path.clone()))
            })
    }

    /// Whether `slug` is this device's feedback board, whose coordinator also triages.
    fn triages(&self, slug: &BoardSlug) -> bool {
        *slug == self.ctx.config.feedback_board()
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
        let started = self.start_or_find_thread(slug, &project, now).await;
        let thread = match started {
            Ok(thread) => thread,
            Err(err) => {
                self.release(slug).await;
                return Err(err);
            }
        };
        let agent = match thread_agent(&thread) {
            Ok(agent) => agent,
            Err(err) => {
                self.release(slug).await;
                return Err(err.context(format!("t3 started thread {thread:?}; archive it in T3")));
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
        self.state.pending_starts.remove(slug);
        self.save()?;
        self.adopt(board, local, now).await
    }

    /// The coordinator thread for `slug`: the one an unfinished earlier start created, if
    /// any, else a new one. An earlier start is unfinished when its pending mark is still
    /// saved (the daemon was killed, or `t3` failed or timed out, after the mark was written);
    /// a thread with the coordinator title created since then is its thread. One that never
    /// ran is briefed first. Threads older than the mark are never taken.
    async fn start_or_find_thread(
        &mut self,
        slug: &BoardSlug,
        project: &Path,
        now: Timestamp,
    ) -> Result<String> {
        let title = coordinator_title(slug);
        let message = coordinator_message(slug, self.triages(slug));
        if let Some(&since) = self.state.pending_starts.get(slug) {
            let found = self
                .t3
                .threads_titled(project, &title)
                .await
                .context("looking for the thread of an unfinished coordinator start")?
                .into_iter()
                .filter(|t| t.created_at >= since)
                .max_by_key(|t| t.created_at);
            if let Some(thread) = found {
                if thread.ran {
                    tracing::warn!(
                        "coordinator of {slug}: adopting thread {}, which a start at {since} launched but did not record",
                        thread.id
                    );
                    return Ok(thread.id);
                }
                tracing::warn!(
                    "coordinator of {slug}: briefing thread {}, which a start at {since} created but did not send its message",
                    thread.id
                );
                if let Err(err) = self.t3.send(&thread.id, &message).await {
                    // Not that thread again: the next start makes a new one.
                    self.state.pending_starts.remove(slug);
                    return Err(err.context(format!(
                        "briefing thread {} left by an earlier start; archive it in T3",
                        thread.id
                    )));
                }
                return Ok(thread.id);
            }
        }
        // On disk before T3 can create the thread.
        self.state.pending_starts.entry(slug.clone()).or_insert(now);
        self.save()?;
        self.t3.start_thread(project, &title, &message).await
    }

    /// Makes the existing thread named by `board board adopt` the board's coordinator: like
    /// [`Self::start_coordinator`], but it briefs that thread instead of starting one. From
    /// then on the thread is this device's coordinator like any other. A thread T3 no longer
    /// has drops the adoption, and a new thread is started. An agent the thread's own hooks
    /// registered keeps its record (where it works, a usage-limit pause); only one the board
    /// has never seen is registered here.
    async fn adopt_thread(
        &mut self,
        board: &Board,
        adoption: Adoption,
        now: Timestamp,
    ) -> Result<()> {
        let slug = &board.slug;
        if self.project_for(board).is_none() {
            anyhow::bail!(no_project(board));
        }
        let agent = match thread_agent(&adoption.thread) {
            Ok(agent) => agent,
            Err(err) => {
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                return Err(err.context(format!("dropped the adoption for {slug}")));
            }
        };
        // Claimed before asking T3, which is slow: meanwhile no other device starts a thread.
        match self.claim(slug, None).await {
            Ok(updated) => {
                self.boards.insert(slug.clone(), updated);
            }
            // Another device holds the lease; the adoption waits until it lets go.
            Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => return Ok(()),
            Err(err) => return Err(err),
        }
        let checked = async {
            let live = self
                .t3
                .thread_info(&adoption.thread)
                .await?
                .is_some_and(|t| !t.archived);
            if !live {
                return Ok(None);
            }
            match self.client().agent_info(&agent).await {
                Ok(_) => Ok(Some(true)),
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Ok(Some(false)),
                Err(err) => Err(err),
            }
        }
        .await;
        let registered = match checked {
            Ok(Some(registered)) => registered,
            Ok(None) => {
                tracing::warn!(
                    "coordinator of {slug}: thread {} to adopt is gone or archived; starting a new thread",
                    adoption.thread
                );
                drop_adoption(&self.ctx.home, slug, &adoption)?;
                return self.start_coordinator(board, now).await;
            }
            Err(err) => {
                self.release(slug).await;
                return Err(err);
            }
        };
        if let Err(err) = self
            .t3
            .send(
                &adoption.thread,
                &adoption_message(slug, self.triages(slug)),
            )
            .await
        {
            self.release(slug).await;
            return Err(err);
        }
        tracing::info!(
            "adopted thread {} as coordinator of {slug}",
            adoption.thread
        );
        let local = Coordinator {
            thread: adoption.thread.clone(),
            agent,
            started_at: now,
            renewed_at: now,
            resumed_after: None,
            registered,
        };
        // Recorded before the adoption is dropped, so the brief is sent only once.
        self.state.coordinators.insert(slug.clone(), local.clone());
        self.state.pending_starts.remove(slug);
        self.save()?;
        drop_adoption(&self.ctx.home, slug, &adoption)?;
        self.adopt(board, local, now).await
    }

    /// Handles one event: queues its wake-ups and advances `last_seq`. Returns true when the
    /// boards should be reconciled. On an error nothing is queued and `last_seq` stays.
    pub async fn on_event(&mut self, event: &Event, now: Timestamp) -> Result<bool> {
        if event.event_type == EventType::AgentStateChanged {
            self.note_state(event, now);
        }
        let (reconcile, wakes) = self.examine(event).await?;
        if let Some(board) = &event.board {
            for (agent, about) in wakes {
                let wake = Wake {
                    board: board.clone(),
                    about,
                    queued_at: now,
                    forwarded: false,
                };
                self.queue(agent, wake);
            }
        }
        self.state.last_seq = Some(self.state.last_seq.unwrap_or(0).max(event.seq));
        Ok(reconcile)
    }

    /// Adds `wake` to `agent`'s queue unless the same one is already there.
    fn queue(&mut self, agent: AgentId, wake: Wake) {
        let queue = self.state.wakes.entry(agent).or_default();
        if !queue
            .wakes
            .iter()
            .any(|w| w.board == wake.board && w.about == wake.about)
        {
            queue.wakes.push(wake);
        }
    }

    /// Keeps what is known of an agent with queued wake-ups current: its held wake-ups are
    /// sent on the next flush once it stops running.
    fn note_state(&mut self, event: &Event, now: Timestamp) {
        let Ok(agent) = event.actor.parse::<AgentId>() else {
            return;
        };
        let Some(seen) = self.agents.get_mut(&agent) else {
            return;
        };
        let Ok(state) = serde_json::from_value::<AgentState>(event.payload["to"].clone()) else {
            tracing::warn!("event #{}: agent state change without a state", event.seq);
            return;
        };
        seen.agent.state = state;
        seen.agent.paused_until = serde_json::from_value(event.payload["paused_until"].clone())
            .ok()
            .flatten();
        seen.since = now;
        seen.idled |= state != AgentState::Running;
    }

    /// What one event calls for: a reconcile, and wake-ups by agent. Only reads.
    async fn examine(&self, event: &Event) -> Result<(bool, Vec<(AgentId, WakeAbout)>)> {
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
                    && let Some(coordinator) = self.wakeable(self.coordinator_here(slug), event)
                {
                    let detail = self.client().task(task).await?;
                    let about = if detail.task.ready && detail.task.auto {
                        Some(WakeAbout::Ready { task: task.clone() })
                    } else if event.event_type == EventType::TaskCreated
                        && is_proposal(&detail.task)
                    {
                        Some(WakeAbout::Proposal { task: task.clone() })
                    } else {
                        None
                    };
                    if let Some(about) = about {
                        wakes.push((coordinator.clone(), about));
                    }
                }
            }
            EventType::TaskDone => {
                if let Some(task) = &event.task
                    && let Some(coordinator) = self.wakeable(self.coordinator_here(slug), event)
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
                        let about = WakeAbout::Cleared {
                            done: task.clone(),
                            ready,
                        };
                        wakes.push((coordinator.clone(), about));
                    }
                }
            }
            // The asker; the coordinator only when the asker is no T3 thread. An asker that
            // turns out not to be wakeable when the flush comes is handled there.
            EventType::AskAnswered => {
                let asker = event.payload["agent"]
                    .as_str()
                    .and_then(|a| a.parse::<AgentId>().ok());
                let (Some(ask), Some(asker)) = (event.payload["id"].as_i64(), asker) else {
                    tracing::warn!(
                        "event #{}: ask_answered without its ask or asker",
                        event.seq
                    );
                    return Ok((false, wakes));
                };
                let to = match self.wakeable(Some(&asker), event) {
                    Some(asker) => Some(asker.clone()),
                    None => self
                        .wakeable(self.coordinator_here(slug), event)
                        .filter(|c| **c != asker)
                        .cloned(),
                };
                if let Some(to) = to {
                    wakes.push((to, WakeAbout::Answered { ask, asker }));
                }
            }
            _ => {}
        }
        Ok((false, wakes))
    }

    /// A lease claim this device made (the server records the claiming device).
    fn is_own_claim(&self, event: &Event) -> bool {
        event.payload["device"].as_str() == Some(self.ctx.device.as_str())
            && event.payload["fields"] == serde_json::json!(["coordinator"])
    }

    /// `agent` when it is a T3 thread that did not cause `event`. Whether it is on this
    /// device and awake is checked when the wake-up is sent.
    fn wakeable<'a>(&self, agent: Option<&'a AgentId>, event: &Event) -> Option<&'a AgentId> {
        agent.filter(|a| a.as_str() != event.actor && a.source() == AgentSource::T3)
    }

    /// The coordinator of an autopilot board, when there is one.
    fn coordinator_here(&self, slug: &BoardSlug) -> Option<&AgentId> {
        self.boards
            .get(slug)
            .filter(|b| b.autopilot && b.status == BoardStatus::Active)
            .and_then(|b| b.coordinator.as_ref())
    }

    /// Sends the queued wake-ups, one message per agent, after checking each again. The
    /// sends run side by side, each given [`WAKE_SEND_TIMEOUT`], so a hanging T3 holds the
    /// daemon up once per flush, not once per agent.
    pub async fn flush(&mut self, now: Timestamp) {
        let mut outgoing = Vec::new();
        for (agent, queue) in std::mem::take(&mut self.state.wakes) {
            if let Some(out) = self.prepare(agent, queue, now).await {
                outgoing.push(out);
            }
        }
        let sends = outgoing
            .iter()
            .map(|out| self.send_wake(&out.agent, &out.message));
        let results = futures_util::future::join_all(sends).await;
        for (out, sent) in outgoing.into_iter().zip(results) {
            self.settle(out, sent, now);
        }
        let wakes = &self.state.wakes;
        self.agents.retain(|agent, _| wakes.contains_key(agent));
    }

    /// Decides what to do with `agent`'s wake-ups now. Drops those past their time to live,
    /// keeps those for later (held, paused, backing off, or a check that failed on a
    /// transient error), passes answers the agent cannot be woken for to the coordinator, and
    /// returns the message to send, if any.
    async fn prepare(
        &mut self,
        agent: AgentId,
        mut queue: WakeQueue,
        now: Timestamp,
    ) -> Option<Outgoing> {
        let seen = match self.seen(&agent, now).await {
            Ok(Some(seen)) => seen,
            // Unknown to the server: its answers go to the coordinator.
            Ok(None) => {
                self.forward_answers(&agent, &mut queue.wakes);
                return None;
            }
            Err(err) => {
                if is_transient(&err) {
                    let delay = back_off(&mut queue, now);
                    tracing::warn!("waking {agent}: {err:#}; trying again in {delay}");
                    self.requeue(agent, queue);
                } else {
                    tracing::warn!("waking {agent}: {err:#}; dropped");
                }
                return None;
            }
        };
        // A paused agent's wake-ups live from the reset on.
        let paused_until = match seen.agent.state {
            AgentState::Paused => seen.agent.paused_until,
            _ => None,
        };
        queue.wakes.retain(|w| {
            let from = paused_until.map_or(w.queued_at, |until| until.max(w.queued_at));
            let fresh = now - from < WAKE_TTL;
            if !fresh {
                tracing::warn!("waking {agent}: not delivered in time; dropped");
            }
            fresh
        });
        match Reach::of(&seen, &self.ctx.device, now) {
            Reach::Now => {}
            Reach::Held => {
                self.requeue(agent, queue);
                return None;
            }
            // Kept for the reset; meanwhile the coordinator hears of its answers.
            Reach::Paused => {
                self.forward_answers(&agent, &mut queue.wakes);
                self.requeue(agent, queue);
                return None;
            }
            // Another device's, or not to be woken by the daemon: its answers go to the
            // coordinator, and the rest is not ours to send.
            Reach::Never => {
                self.forward_answers(&agent, &mut queue.wakes);
                return None;
            }
        }
        if queue.backoff.as_ref().is_some_and(|b| now < b.retry_at) {
            self.requeue(agent, queue);
            return None;
        }
        let backoff = queue.backoff.clone();
        let mut sent = Vec::new();
        let mut lines = Vec::new();
        for wake in std::mem::take(&mut queue.wakes) {
            match self.recheck(&agent, &wake).await {
                Ok(Some(line)) => {
                    if !lines.contains(&line) {
                        lines.push(line);
                    }
                    sent.push(wake);
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!("waking {agent}: {err:#}");
                    if is_transient(&err) {
                        queue.wakes.push(wake);
                    }
                }
            }
        }
        self.requeue(agent.clone(), queue);
        if lines.is_empty() {
            return None;
        }
        Some(Outgoing {
            agent,
            message: format!("{}\nRun board_snapshot and continue.", lines.join("\n")),
            wakes: sent,
            backoff,
        })
    }

    /// What is known of `agent`: read from the server the first time, then kept current by
    /// its events. `None` when the server does not know it.
    async fn seen(&mut self, agent: &AgentId, now: Timestamp) -> Result<Option<Seen>> {
        if let Some(seen) = self.agents.get(agent) {
            return Ok(Some(seen.clone()));
        }
        match self.client().agent_info(agent).await {
            Ok(info) => {
                let seen = Seen {
                    agent: info,
                    since: now,
                    idled: false,
                };
                self.agents.insert(agent.clone(), seen.clone());
                Ok(Some(seen))
            }
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Puts `queue` back for a later flush, after wake-ups queued for `agent` meanwhile.
    fn requeue(&mut self, agent: AgentId, queue: WakeQueue) {
        if queue.wakes.is_empty() {
            return;
        }
        let backoff = queue.backoff;
        for wake in queue.wakes {
            self.queue(agent.clone(), wake);
        }
        if let Some(backoff) = backoff
            && let Some(kept) = self.state.wakes.get_mut(&agent)
        {
            kept.backoff = Some(backoff);
        }
    }

    /// Passes the answers to `from`'s own asks, which `from` cannot be woken for now, to the
    /// board's coordinator, once each: an answer no one hears of would stall its task.
    fn forward_answers(&mut self, from: &AgentId, wakes: &mut [Wake]) {
        for wake in wakes {
            let own = matches!(&wake.about, WakeAbout::Answered { asker, .. } if asker == from);
            if !own || wake.forwarded {
                continue;
            }
            wake.forwarded = true;
            let coordinator = self
                .coordinator_here(&wake.board)
                .filter(|c| *c != from && c.source() == AgentSource::T3)
                .cloned();
            if let Some(coordinator) = coordinator {
                let forward = Wake {
                    forwarded: false,
                    ..wake.clone()
                };
                self.queue(coordinator, forward);
            }
        }
    }

    /// Sends `message` to `agent`'s thread; on a failure, asks T3 whether the thread is gone.
    async fn send_wake(&self, agent: &AgentId, message: &str) -> Sent {
        let thread = agent.native();
        let err = match tokio::time::timeout(WAKE_SEND_TIMEOUT, self.t3.send(thread, message)).await
        {
            Ok(Ok(())) => return Sent::Delivered,
            Ok(Err(err)) if err.downcast_ref::<TimedOut>().is_some() => {
                return Sent::MaybeDelivered(err);
            }
            Ok(Err(err)) => err,
            Err(_) => {
                return Sent::MaybeDelivered(TimedOut(format!("t3 thread send {thread}")).into());
            }
        };
        match tokio::time::timeout(WAKE_SEND_TIMEOUT, self.t3.thread_info(thread)).await {
            Ok(Ok(None)) => Sent::Gone(err),
            Ok(Ok(Some(info))) if info.archived => Sent::Gone(err),
            _ => Sent::Failed(err),
        }
    }

    /// Records how sending `out` went.
    fn settle(&mut self, out: Outgoing, sent: Sent, now: Timestamp) {
        let agent = out.agent;
        match sent {
            Sent::Delivered => {
                if let Some(queue) = self.state.wakes.get_mut(&agent) {
                    queue.backoff = None;
                }
            }
            Sent::MaybeDelivered(err) => tracing::warn!(
                "waking {agent}: {err:#}; not sent again, since T3 may have delivered it"
            ),
            Sent::Gone(err) => {
                tracing::warn!(
                    "waking {agent}: {err:#}; its thread is gone, so its wake-ups are dropped"
                );
                let mut wakes = out.wakes;
                if let Some(queue) = self.state.wakes.remove(&agent) {
                    wakes.extend(queue.wakes);
                }
                self.forward_answers(&agent, &mut wakes);
                self.agents.remove(&agent);
            }
            Sent::Failed(err) => {
                let mut queue = WakeQueue {
                    wakes: out.wakes,
                    backoff: out.backoff,
                };
                let delay = back_off(&mut queue, now);
                tracing::warn!("waking {agent}: {err:#}; trying again in {delay}");
                self.requeue(agent, queue);
            }
        }
    }

    /// The line for `wake` when what it is about still holds, else `None`.
    async fn recheck(&self, agent: &AgentId, wake: &Wake) -> Result<Option<String>> {
        let slug = &wake.board;
        let coordinator = self.coordinator_here(slug) == Some(agent);
        let line = match &wake.about {
            WakeAbout::Ready { task } if coordinator => {
                let task = self.client().task(task).await?.task;
                (task.ready && task.auto).then(|| {
                    format!(
                        "board {slug}: {} is ready ({})",
                        task.id,
                        priority(&task.priority)
                    )
                })
            }
            WakeAbout::Cleared { done, ready } if coordinator => {
                let mut still: Vec<TaskId> = Vec::new();
                for id in ready {
                    let t = self.client().task(id).await?.task;
                    if t.ready && t.auto {
                        still.push(t.id);
                    }
                }
                let ids: Vec<&str> = still.iter().map(TaskId::as_str).collect();
                (!ids.is_empty()).then(|| {
                    format!(
                        "board {slug}: {done} is done; its blocker cleared for {}",
                        ids.join(", ")
                    )
                })
            }
            WakeAbout::Proposal { task } if coordinator => {
                let task = self.client().task(task).await?.task;
                is_proposal(&task)
                    .then(|| format!("board {slug}: {} is a proposal to review", task.id))
            }
            // An answer stays answered; only its text is read.
            WakeAbout::Answered { ask, asker } if asker == agent || coordinator => {
                let ask = self.client().ask(*ask).await?;
                let by = if asker == agent {
                    String::new()
                } else {
                    format!(" by {asker}")
                };
                let about = ask
                    .task
                    .as_ref()
                    .map(|t| format!(" about {t}"))
                    .unwrap_or_default();
                let answer = serde_json::to_string(ask.answer.as_deref().unwrap_or_default())?
                    .replace('<', "\\u003c")
                    .replace('>', "\\u003e");
                Some(format!(
                    "the user answered ask #{}{by}{about}: {answer}",
                    ask.id
                ))
            }
            // The agent is no longer this board's coordinator.
            WakeAbout::Ready { .. }
            | WakeAbout::Cleared { .. }
            | WakeAbout::Proposal { .. }
            | WakeAbout::Answered { .. } => None,
        };
        Ok(line)
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

/// Backs `queue` off after a failure: nothing is sent before the returned delay passes.
fn back_off(queue: &mut WakeQueue, now: Timestamp) -> TimeDelta {
    let last = queue
        .backoff
        .as_ref()
        .map(|b| TimeDelta::seconds(b.delay_secs));
    let delay = next_delay(last, WAKE_RETRY_FIRST, WAKE_RETRY_MAX);
    queue.backoff = Some(Backoff {
        delay_secs: delay.num_seconds(),
        retry_at: now + delay,
    });
    delay
}

/// Is this a failure that may pass on its own: no answer, or a server-side error?
fn is_transient(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<ServerError>() {
        Some(server) => !server.status.is_client_error(),
        None => true,
    }
}

fn no_project(board: &Board) -> String {
    if board.repos.is_empty() {
        return format!(
            "no T3 project for board {} on this device: add it under [board_projects] in config.toml",
            board.slug
        );
    }
    format!(
        "no T3 project for {} on this device: add it under [projects] (or the board under \
         [board_projects]) in config.toml",
        board.repos.join(", ")
    )
}

/// A pending proposal: open, not `auto`, with a body starting `Proposal:`
/// (prompts/coordinator.md "Proposals"). Its filing wakes the coordinator to review it.
fn is_proposal(task: &board_proto::Task) -> bool {
    task.status == board_proto::TaskStatus::Open
        && !task.auto
        && task.body.trim_start().starts_with("Proposal:")
}

fn priority(p: &Priority) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The title of a board's coordinator thread.
pub fn coordinator_title(slug: &BoardSlug) -> String {
    format!("{slug} coordinator")
}

/// The coordinator's standing instructions: [`COORDINATOR_PROMPT`], followed by
/// [`TRIAGE_PROMPT`] when the board is the feedback board.
fn standing_prompt(triage: bool) -> String {
    if triage {
        format!("{COORDINATOR_PROMPT}\n{TRIAGE_PROMPT}")
    } else {
        COORDINATOR_PROMPT.to_owned()
    }
}

/// The first message of a coordinator thread; `triage` for the device's feedback board.
pub fn coordinator_message(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!("{prompt}\nYou are the coordinator of board {slug}. Start now: call board_snapshot.")
}

/// The message that briefs an existing thread adopted as a board's coordinator; `triage`
/// for the device's feedback board.
pub fn adoption_message(slug: &BoardSlug, triage: bool) -> String {
    let prompt = standing_prompt(triage);
    format!(
        "{prompt}\nThis thread is now also the coordinator of board {slug}: `board board adopt` named it, so the board's ready work comes to you from here on, and the board wakes you when work becomes ready, one of your asks is answered, or an ask is answered whose asker it cannot wake. Keep what you already know about this work. A board task you already own counts as the board's task in progress: finish it first. Start now: call board_snapshot."
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
        daemon.flush(Utc::now()).await;
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
                        self.flush(Utc::now()).await;
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
        self.flush(Utc::now()).await;
        self.save()
    }

    async fn reconcile_logged(&mut self) {
        if let Err(err) = self.reconcile(Utc::now()).await {
            tracing::warn!("reconcile: {err:#}");
        }
    }
}
