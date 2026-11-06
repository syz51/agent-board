//! Board operations shared by the CLI and the MCP server: each returns concise text and
//! the JSON the server answered.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use board_proto::{
    AddNote, AgentId, AgentRole, AgentSource, AskKind, Board, BoardSlug, BoardStatus, CreateAsk,
    CreateBoard, CreateLeases, CreateTask, DeviceName, DoneTask, ErrorCode, EvidenceKind,
    NewEvidence, NoteKind, Priority, PutHandoff, RegisterAgent, Runtime, SnapshotFormat,
    SnapshotKind, TaskId, TaskStatus, UpdateBoard, UpdateTask,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::client::{Client, SnapshotQuery, TaskQuery, error_code};
use crate::config::{Config, Home, load_token};
use crate::identity::RepoInfo;
use crate::render;

/// Everything a command needs: configuration, a client and where it runs.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub home: Home,
    pub config: Config,
    pub device: DeviceName,
    pub client: Client,
    pub repo: RepoInfo,
}

impl Ctx {
    /// Loads the configuration and token from `home` and builds a client for `agent`.
    pub fn load(home: Home, agent: Option<AgentId>, timeout: Option<Duration>) -> Result<Self> {
        let config = Config::load(&home)?;
        let (server, device) = config.require_login()?;
        let token =
            load_token(&home)?.context("no device token: run `board login`, or set BOARD_TOKEN")?;
        let client = Client::new(server, token, agent, timeout)?;
        let device = device.clone();
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(Self {
            home,
            config,
            device,
            client,
            repo: RepoInfo::detect(&cwd),
        })
    }

    pub fn agent(&self) -> Result<&AgentId> {
        self.client.require_agent()
    }

    /// The same context acting as another agent.
    pub fn as_agent(&self, agent: Option<AgentId>) -> Self {
        Self {
            client: self.client.with_agent(agent),
            ..self.clone()
        }
    }

    /// Registers (or refreshes) the calling agent with this device's repo and branch.
    pub async fn register(&self, runtime: Runtime, native: Vec<(String, String)>) -> Result<()> {
        let agent = self.agent()?;
        self.client
            .register(&RegisterAgent {
                id: agent.clone(),
                runtime,
                device: self.device.clone(),
                role: AgentRole::Worker,
                title: None,
                repo: self.repo.key.clone(),
                branch: self.repo.branch.clone(),
                cwd: Some(self.repo.cwd.display().to_string()),
                native_ids: native.into_iter().collect(),
                forked_from: None,
            })
            .await?;
        Ok(())
    }

    /// Registers the calling agent when the server does not know it yet: a CLI or MCP
    /// call from a session whose start hook did not run. A known agent is left as it is.
    pub async fn ensure_registered(&self) -> Result<()> {
        let Some(agent) = self.client.agent() else {
            return Ok(());
        };
        match self.client.agent_info(agent).await {
            Ok(_) => Ok(()),
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                let runtime = match agent.source() {
                    AgentSource::Codex => Runtime::Codex,
                    AgentSource::T3 | AgentSource::Claude => Runtime::Claude,
                };
                self.register(runtime, Vec::new()).await
            }
            Err(err) => Err(err),
        }
    }

    /// The board to act on: the explicit one, else the agent's only board (or its board
    /// for this repo), else the one active board covering this repo.
    pub async fn resolve_board(&self, explicit: Option<BoardSlug>) -> Result<BoardSlug> {
        if let Some(board) = explicit {
            return Ok(board);
        }
        let boards = self.client.boards().await?;
        let for_repo = |b: &Board| {
            self.repo
                .key
                .as_ref()
                .is_some_and(|k| b.repos.iter().any(|r| r == k))
        };
        if let Some(agent) = self.client.agent()
            && let Ok(info) = self.client.agent_info(agent).await
        {
            let mine: Vec<&Board> = boards
                .iter()
                .filter(|b| info.boards.contains(&b.slug))
                .collect();
            if let [only] = mine.as_slice() {
                return Ok(only.slug.clone());
            }
            if let [only] = mine
                .iter()
                .filter(|b| for_repo(b))
                .collect::<Vec<_>>()
                .as_slice()
            {
                return Ok(only.slug.clone());
            }
        }
        let active: Vec<&Board> = boards
            .iter()
            .filter(|b| for_repo(b))
            .filter(|b| b.status == BoardStatus::Active)
            .collect();
        match active.as_slice() {
            [only] => Ok(only.slug.clone()),
            [] => bail!("no board for this repo: pass a board (see `board boards`)"),
            many => {
                let names: Vec<&str> = many.iter().map(|b| b.slug.as_str()).collect();
                bail!(
                    "several boards cover this repo ({}): pass one",
                    names.join(", ")
                )
            }
        }
    }
}

/// A result as text for people and agents, and as JSON for scripts.
#[derive(Debug, Clone)]
pub struct Output {
    pub text: String,
    pub json: Value,
}

impl Output {
    fn new<T: Serialize>(text: String, value: &T) -> Self {
        Self {
            text,
            json: serde_json::to_value(value).expect("wire types serialize"),
        }
    }
}

/// Parses a `snake_case` enum value the way the wire format spells it.
pub fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(value.to_owned()))
        .map_err(|_| format!("unknown value {value:?}"))
}

pub async fn snapshot(
    ctx: &Ctx,
    kind: SnapshotKind,
    format: SnapshotFormat,
    since: Option<i64>,
) -> Result<Output> {
    let snap = ctx
        .client
        .snapshot(&SnapshotQuery {
            agent: ctx.agent()?.clone(),
            kind,
            since,
            format,
        })
        .await?;
    if kind == SnapshotKind::Full {
        // Best effort: the cache only serves the offline fallback.
        let _ = crate::state::save_cached_snapshot(&ctx.home, ctx.agent()?, &snap);
    }
    let text = if snap.text.is_empty() {
        "nothing to report".to_owned()
    } else {
        snap.text.clone()
    };
    Ok(Output::new(text, &snap))
}

pub async fn status(ctx: &Ctx, board: Option<BoardSlug>, detailed: bool) -> Result<Output> {
    let mut lines = Vec::new();
    let me = match ctx.client.agent() {
        Some(agent) => format!("you: {agent}"),
        None => "you: no agent id (device only)".to_owned(),
    };
    lines.push(format!(
        "{me} · device {} · repo {} · branch {}",
        ctx.device,
        ctx.repo.key.as_deref().unwrap_or("-"),
        ctx.repo.branch.as_deref().unwrap_or("-")
    ));
    let board = match board {
        Some(b) => Some(b),
        None => ctx.resolve_board(None).await.ok(),
    };
    let json = match board {
        Some(slug) => {
            let b = ctx.client.board(&slug).await?;
            let agents = ctx.client.agents(Some(&slug)).await?;
            lines.push(render::board_detail(&b, &agents, detailed));
            json!({"board": b, "agents": agents})
        }
        None => {
            let boards = ctx.client.boards().await?;
            lines.push(render::board_list(&boards));
            json!({"boards": boards})
        }
    };
    Ok(Output {
        text: lines.join("\n"),
        json,
    })
}

pub async fn task_list(ctx: &Ctx, query: &TaskQuery, detailed: bool) -> Result<Output> {
    let tasks = ctx.client.tasks(query).await?;
    let text = if detailed {
        render::task_list_detailed(&tasks)
    } else {
        render::task_list(&tasks)
    };
    Ok(Output::new(text, &tasks))
}

pub async fn task_show(ctx: &Ctx, id: &TaskId, detailed: bool) -> Result<Output> {
    let detail = ctx.client.task(id).await?;
    Ok(Output::new(render::task_detail(&detail, detailed), &detail))
}

#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub board: Option<BoardSlug>,
    pub title: String,
    pub body: String,
    pub acceptance: String,
    pub parent: Option<TaskId>,
    pub blockers: Vec<TaskId>,
    pub priority: Option<Priority>,
    pub auto: Option<bool>,
}

pub async fn task_add(ctx: &Ctx, new: NewTask) -> Result<Output> {
    let board = match (&new.board, &new.parent) {
        (None, Some(parent)) => parent.board(),
        _ => ctx.resolve_board(new.board.clone()).await?,
    };
    let task = ctx
        .client
        .create_task(&CreateTask {
            board,
            agent: ctx.agent()?.clone(),
            title: new.title,
            body: new.body,
            acceptance: new.acceptance,
            parent: new.parent,
            blockers: new.blockers,
            priority: new.priority.unwrap_or(Priority::P2),
            auto: new.auto,
        })
        .await?;
    Ok(Output::new(
        format!("added {}", render::task_line(&task)),
        &task,
    ))
}

/// `claim`, `release` or `take`.
pub async fn task_action(ctx: &Ctx, id: &TaskId, action: &str) -> Result<Output> {
    let task = ctx.client.task_action(id, action).await?;
    let verb = match action {
        "claim" => "claimed",
        "release" => "released",
        _ => "took",
    };
    Ok(Output::new(
        format!("{verb} {}", render::task_line(&task)),
        &task,
    ))
}

#[derive(Debug, Clone, Default)]
pub struct TaskChanges {
    pub status: Option<TaskStatus>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub acceptance: Option<String>,
    pub priority: Option<Priority>,
    pub blockers: Option<Vec<TaskId>>,
    pub blocked_reason: Option<String>,
    pub auto: Option<bool>,
}

/// Reads the task's version, then applies the changes.
pub async fn task_update(ctx: &Ctx, id: &TaskId, changes: TaskChanges) -> Result<Output> {
    let version = ctx.client.task(id).await?.task.version;
    let task = ctx
        .client
        .update_task(
            id,
            &UpdateTask {
                agent: ctx.agent()?.clone(),
                version,
                status: changes.status,
                title: changes.title,
                body: changes.body,
                acceptance: changes.acceptance,
                priority: changes.priority,
                blockers: changes.blockers,
                blocked_reason: changes.blocked_reason,
                auto: changes.auto,
            },
        )
        .await?;
    Ok(Output::new(
        format!("updated {}", render::task_line(&task)),
        &task,
    ))
}

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    /// Commands run, with their exit codes.
    pub checks: Vec<(String, i32)>,
    pub commits: Vec<String>,
    pub prs: Vec<String>,
    pub artifacts: Vec<String>,
}

impl Evidence {
    fn into_wire(self) -> Vec<NewEvidence> {
        let item = |kind, value, exit_code| NewEvidence {
            kind,
            value,
            exit_code,
        };
        let mut out: Vec<NewEvidence> = self
            .checks
            .into_iter()
            .map(|(cmd, code)| item(EvidenceKind::Check, cmd, Some(code)))
            .collect();
        out.extend(
            self.commits
                .into_iter()
                .map(|v| item(EvidenceKind::Commit, v, None)),
        );
        out.extend(
            self.prs
                .into_iter()
                .map(|v| item(EvidenceKind::Pr, v, None)),
        );
        out.extend(
            self.artifacts
                .into_iter()
                .map(|v| item(EvidenceKind::Artifact, v, None)),
        );
        out
    }
}

pub async fn task_done(ctx: &Ctx, id: &TaskId, evidence: Evidence) -> Result<Output> {
    let evidence = evidence.into_wire();
    if evidence.is_empty() {
        bail!(
            "done needs evidence: a check with its exit code (--check <cmd> --exit <code>), a commit, a PR or an artifact"
        );
    }
    let version = ctx.client.task(id).await?.task.version;
    let task = ctx
        .client
        .done(
            id,
            &DoneTask {
                agent: ctx.agent()?.clone(),
                version,
                evidence,
            },
        )
        .await?;
    Ok(Output::new(
        format!("done {}", render::task_line(&task)),
        &task,
    ))
}

pub async fn task_note(ctx: &Ctx, id: &TaskId, kind: NoteKind, text: String) -> Result<Output> {
    let note = ctx
        .client
        .add_note(
            id,
            &AddNote {
                agent: ctx.agent()?.clone(),
                kind,
                text,
            },
        )
        .await?;
    Ok(Output::new(
        format!("noted on {} (#{})", note.task, note.id),
        &note,
    ))
}

/// Replaces the caller's handoff, reading its version first.
pub async fn handoff_set(ctx: &Ctx, now: String, next: String, blockers: String) -> Result<Output> {
    let agent = ctx.agent()?;
    let version = ctx.client.handoff(agent).await?.map(|h| h.version);
    let handoff = ctx
        .client
        .put_handoff(
            agent,
            &PutHandoff {
                now,
                next,
                blockers,
                version,
            },
        )
        .await?;
    Ok(Output::new(
        format!("handoff saved (version {})", handoff.version),
        &handoff,
    ))
}

pub async fn handoff_show(ctx: &Ctx, of: Option<AgentId>) -> Result<Output> {
    let me = ctx.client.agent().cloned();
    let agent = match of.or(me.clone()) {
        Some(agent) => agent,
        None => bail!("no agent id: pass one"),
    };
    match ctx.client.handoff(&agent).await? {
        Some(h) => Ok(Output::new(
            render::handoff(&h, me.as_ref() == Some(&agent)),
            &h,
        )),
        None => Ok(Output {
            text: format!("{agent} has no handoff"),
            json: Value::Null,
        }),
    }
}

pub async fn lease_paths(
    ctx: &Ctx,
    globs: Vec<String>,
    task: Option<TaskId>,
    ttl_secs: Option<u32>,
) -> Result<Output> {
    let repo = ctx
        .repo
        .key
        .clone()
        .context("not in a git repo with an origin remote: run from the repo")?;
    let result = ctx
        .client
        .lease(&CreateLeases {
            agent: ctx.agent()?.clone(),
            repo,
            globs,
            task,
            ttl_secs,
        })
        .await?;
    Ok(Output::new(render::lease_result(&result), &result))
}

pub struct NewAsk {
    pub board: Option<BoardSlug>,
    pub task: Option<TaskId>,
    pub kind: AskKind,
    pub question: String,
    pub options: Vec<String>,
}

pub async fn ask_user(ctx: &Ctx, ask: NewAsk) -> Result<Output> {
    let board = match (&ask.board, &ask.task) {
        (None, Some(task)) => task.board(),
        _ => ctx.resolve_board(ask.board.clone()).await?,
    };
    let created = ctx
        .client
        .create_ask(&CreateAsk {
            agent: ctx.agent()?.clone(),
            board,
            task: ask.task,
            kind: ask.kind,
            question: ask.question,
            options: ask.options,
        })
        .await?;
    Ok(Output::new(
        format!(
            "{}; the user is notified. Continue with other ready work; the answer wakes you.",
            render::ask_line(&created)
        ),
        &created,
    ))
}

pub async fn asks(ctx: &Ctx, status: Option<&str>, board: Option<&BoardSlug>) -> Result<Output> {
    let asks = ctx.client.asks(status, board).await?;
    Ok(Output::new(render::ask_list(&asks), &asks))
}

pub async fn answer(ctx: &Ctx, id: i64, text: &str) -> Result<Output> {
    let ask = ctx.client.answer(id, text).await?;
    Ok(Output::new(
        format!("answered {}", render::ask_line(&ask)),
        &ask,
    ))
}

pub async fn join(ctx: &Ctx, board: Option<BoardSlug>) -> Result<Output> {
    let slug = ctx.resolve_board(board).await?;
    let board = ctx.client.join(&slug).await?;
    Ok(Output::new(
        format!("joined {}", render::board_line(&board)),
        &board,
    ))
}

pub async fn boards(ctx: &Ctx) -> Result<Output> {
    let boards = ctx.client.boards().await?;
    Ok(Output::new(render::board_list(&boards), &boards))
}

pub async fn board_create(ctx: &Ctx, body: CreateBoard) -> Result<Output> {
    let board = ctx.client.create_board(&body).await?;
    Ok(Output::new(
        format!("created {}", render::board_line(&board)),
        &board,
    ))
}

pub async fn board_update(ctx: &Ctx, slug: &BoardSlug, body: UpdateBoard) -> Result<Output> {
    let board = ctx.client.update_board(slug, &body).await?;
    Ok(Output::new(
        format!("updated {}", render::board_line(&board)),
        &board,
    ))
}

pub async fn log(
    ctx: &Ctx,
    since: Option<i64>,
    board: Option<&BoardSlug>,
    limit: Option<u32>,
) -> Result<Output> {
    let page = ctx.client.events(since, board, limit).await?;
    Ok(Output::new(render::event_list(&page.events), &page))
}

pub async fn stats(ctx: &Ctx, days: u32) -> Result<Output> {
    let stats = ctx.client.stats(days).await?;
    Ok(Output::new(render::stats(&stats), &stats))
}

/// Files feedback about the board as a task on the feedback board.
pub async fn feedback(ctx: &Ctx, text: String) -> Result<Output> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        bail!("feedback is empty");
    }
    let first = text.lines().next().unwrap_or_default();
    let title: String = if first.chars().count() > 72 {
        first.chars().take(71).chain(['…']).collect()
    } else {
        first.to_owned()
    };
    task_add(
        ctx,
        NewTask {
            board: Some(ctx.config.feedback_board()),
            title,
            body: text,
            ..NewTask::default()
        },
    )
    .await
}
