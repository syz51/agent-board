//! Board operations shared by the CLI and the MCP server: each returns concise text and
//! the JSON the server answered.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use board_proto::{
    AddNote, AgentId, AgentRole, AgentSource, AgentState, AskKind, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, CreateAsk, CreateBoard, CreateFeedback, CreateLeases, CreateTask, DeviceName,
    DoneTask, ErrorCode, EvidenceKind, NewEvidence, NoteKind, Priority, PutHandoff, RegisterAgent,
    ResolveFeedback, Runtime, SnapshotFormat, SnapshotKind, Task, TaskId, TaskStatus, Timestamp,
    UpdateBoard, UpdateTask,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::client::{Client, ErrorsQuery, SnapshotQuery, TaskQuery, error_code};
use crate::config::{Config, Home, load_token};
use crate::daemon::{self, Adoption};
use crate::identity::RepoInfo;
use crate::render;
use crate::t3::T3;

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

/// A `not_before` argument: absent leaves it alone (`None`); `none` clears it
/// (`Some(None)`); otherwise a date (`YYYY-MM-DD`, midnight UTC) or an RFC 3339 time.
pub fn parse_not_before(value: Option<&str>) -> Result<Option<Option<Timestamp>>> {
    let Some(value) = value.map(str::trim) else {
        return Ok(None);
    };
    if value.eq_ignore_ascii_case("none") {
        return Ok(Some(None));
    }
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(Some(Some(at.to_utc())));
    }
    match chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        Ok(date) => Ok(Some(Some(date.and_time(chrono::NaiveTime::MIN).and_utc()))),
        Err(_) => bail!(
            "not_before {value:?} is neither a date (YYYY-MM-DD) nor an RFC 3339 time; send one, or none to clear it"
        ),
    }
}

/// Text limits from the server defaults (`docs/API.md`, "Defaults"), which the MCP schemas
/// state and the CLI checks before sending.
pub fn limits() -> &'static board_server::Settings {
    static LIMITS: std::sync::LazyLock<board_server::Settings> =
        std::sync::LazyLock::new(board_server::Settings::default);
    &LIMITS
}

/// Refuses `value` when it has more than `max` characters, with the server's message and
/// hint, so over-long text fails before a request is sent.
pub fn check_chars(field: &str, value: &str, max: usize) -> Result<()> {
    check_chars_or(field, value, max, board_proto::longer_text_goes(field))
}

/// [`check_chars`] with `goes` saying where longer text goes, as the server says it.
pub fn check_chars_or(field: &str, value: &str, max: usize, goes: Option<&str>) -> Result<()> {
    match board_proto::TooLong::check(field, value, max) {
        Some(long) => bail!(
            "{} (not sent)\nnext: {}",
            long.message(),
            long.next_or(goes)
        ),
        None => Ok(()),
    }
}

/// Parses a `snake_case` enum value the way the wire format spells it.
pub fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(value.to_owned())).map_err(|_| {
        format!(
            "unknown value {value:?}; send one of {}",
            enum_values::<T>().join(", ")
        )
    })
}

/// The wire spellings of a unit-variant enum, read from its derived `Deserialize`, which
/// names them when it asks the deserializer for an enum.
pub fn enum_values<T: DeserializeOwned>() -> &'static [&'static str] {
    struct Probe<'a>(&'a mut &'static [&'static str]);

    impl<'de> serde::Deserializer<'de> for Probe<'_> {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _: V,
        ) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom("not an enum"))
        }

        fn deserialize_enum<V: serde::de::Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = variants;
            Err(serde::de::Error::custom("variants read"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map struct
            identifier ignored_any
        }
    }

    let mut variants: &'static [&'static str] = &[];
    let _ = T::deserialize(Probe(&mut variants));
    variants
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
    pub not_before: Option<Timestamp>,
    /// Create it in doing, owned by this agent.
    pub claim: bool,
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
            not_before: new.not_before,
            claim: new.claim,
        })
        .await?;
    if !new.claim {
        let mut text = format!("added {}", render::task_line(&task));
        if task.is_pending_proposal() {
            text.push_str(
                "\npending proposal: not ready work. Another agent reviews it with a `verdict:` \
                 note; an accept sets auto, which makes it claimable.",
            );
        }
        return Ok(Output::new(text, &task));
    }
    // A server that predates `claim` ignores it and creates the task open: claim it now,
    // which another agent may win in between.
    let task = if task.status == TaskStatus::Open {
        ctx.client
            .task_action(&task.id, "claim")
            .await
            .with_context(|| {
                format!(
                    "added {} but could not claim it; the server predates claim on create",
                    task.id
                )
            })?
    } else {
        task
    };
    Ok(Output::new(
        format!("added and claimed {}", render::task_line(&task)),
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
    let mut text = format!("{verb} {}", render::task_line(&task));
    if action != "release" {
        // The task is the agent's now either way, so a failed read only loses the count.
        let notes = match ctx.client.task(id).await {
            Ok(detail) => render::claim_notes(&detail),
            Err(err) => {
                tracing::warn!("reading {id}'s notes after the {action}: {err:#}");
                format!("task_show {id} and read every note before starting")
            }
        };
        text.push('\n');
        text.push_str(&notes);
    }
    Ok(Output::new(text, &task))
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
    /// `Some(None)` clears it.
    pub not_before: Option<Option<Timestamp>>,
    /// Hand the task to this agent.
    pub owner: Option<AgentId>,
}

/// Reads the task's version, then applies the changes.
pub async fn task_update(ctx: &Ctx, id: &TaskId, changes: TaskChanges) -> Result<Output> {
    let version = ctx.client.task(id).await?.task.version;
    let wanted_owner = changes.owner.clone();
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
                not_before: changes.not_before,
                owner: changes.owner,
            },
        )
        .await?;
    check_owner(&task, wanted_owner.as_ref())?;
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

pub async fn task_done(
    ctx: &Ctx,
    id: &TaskId,
    evidence: Evidence,
    summary: Option<String>,
) -> Result<Output> {
    let evidence = evidence.into_wire();
    if evidence.is_empty() {
        bail!(
            "done needs evidence: a check with its exit code (--check <cmd> --exit <code>), a commit, a PR or an artifact"
        );
    }
    let detail = ctx.client.task(id).await?;
    let task = ctx
        .client
        .done(
            id,
            &DoneTask {
                agent: ctx.agent()?.clone(),
                version: detail.task.version,
                evidence,
                summary,
            },
        )
        .await?;
    // For the Stop hook's reminder to file what remains (`hook::done_reminder`). The server
    // has the task done either way, so a failed write costs only the reminder.
    if let Err(err) =
        crate::state::record_done(&ctx.home, ctx.agent()?, &task.id, chrono::Utc::now())
    {
        tracing::warn!("recording {} done for the Stop hook: {err:#}", task.id);
    }
    let mut text = format!("done {}", render::task_line(&task));
    // Read again: a note does not change the version, so one added after the read above
    // would be missed. When the read fails, the one above still lists the earlier ones.
    let detail = match ctx.client.task(id).await {
        Ok(after) => after,
        Err(err) => {
            tracing::warn!("reading {id}'s findings after done: {err:#}");
            detail
        }
    };
    let findings = render::done_findings(&detail, ctx.agent()?.as_str());
    if !findings.is_empty() {
        text.push('\n');
        text.push_str(&findings);
    }
    Ok(Output::new(text, &task))
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
    let agent = ctx.agent()?.clone();
    let answer = ctx
        .client
        .create_ask(&CreateAsk {
            agent: agent.clone(),
            board,
            task: ask.task,
            kind: ask.kind,
            question: ask.question,
            options: ask.options,
        })
        .await?;
    let created = &answer.ask;
    let mut text = format!(
        "{}; the user is notified. Continue with other ready work; the answer wakes you.",
        render::ask_line(created)
    );
    // The server's warning names the board's other recent open asks, which this may repeat.
    if let Some(warning) = &answer.warning {
        text.push_str(&format!("\nwarning: {warning}"));
    }
    if let Some(task) = &created.task {
        // Advisory: the ask is made either way, so a failed check is reported, not raised.
        match ctx.client.asks(Some("open"), Some(&created.board)).await {
            Ok(open) => {
                let others: Vec<_> = open
                    .iter()
                    .filter(|other| other.id != created.id && other.task.as_ref() == Some(task))
                    .collect();
                if !others.is_empty() {
                    let named: Vec<String> = others
                        .iter()
                        .map(|other| {
                            let by = if other.agent == agent {
                                "you".to_owned()
                            } else {
                                other.agent.to_string()
                            };
                            format!("#{} by {by}", other.id)
                        })
                        .collect();
                    text.push_str(&format!(
                        "\nwarning: {task} already has open {}: {}.",
                        if others.len() == 1 { "ask" } else { "asks" },
                        named.join(", ")
                    ));
                    if others.iter().any(|other| other.agent == agent) {
                        text.push_str(
                            " Withdraw yours that this one replaces (ask_withdraw; CLI: board withdraw <id>).",
                        );
                    }
                }
            }
            Err(err) => text.push_str(&format!(
                "\nwarning: could not check {task} for other open asks: {err:#}"
            )),
        }
    }
    Ok(Output::new(text, &answer))
}

pub async fn ask_withdraw(ctx: &Ctx, id: i64) -> Result<Output> {
    let ask = ctx.client.withdraw_ask(id).await?;
    Ok(Output::new(
        format!("withdrew {}", render::ask_line(&ask)),
        &ask,
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
    check_temporary(&board, body.temporary, "created")?;
    Ok(Output::new(
        format!("created {}", render::board_line(&board)),
        &board,
    ))
}

pub async fn board_update(ctx: &Ctx, slug: &BoardSlug, body: UpdateBoard) -> Result<Output> {
    let board = ctx.client.update_board(slug, &body).await?;
    if let Some(temporary) = body.temporary {
        check_temporary(&board, temporary, "updated")?;
    }
    Ok(Output::new(
        format!("updated {}", render::board_line(&board)),
        &board,
    ))
}

/// A server that predates handovers ignores `owner` and answers with the owner unchanged,
/// so the new owner's `task_done` would be refused later.
fn check_owner(task: &Task, wanted: Option<&AgentId>) -> Result<()> {
    if let Some(wanted) = wanted
        && task.owner.as_ref() != Some(wanted)
    {
        bail!(
            "updated {id}, but the server ignored owner={wanted}: it predates handovers, so \
             {owner} still owns it. Upgrade the server, then hand it over again",
            id = task.id,
            owner = task.owner.as_ref().map_or("nobody", AgentId::as_str),
        );
    }
    Ok(())
}

/// A server that predates temporary boards ignores the flag, so a board meant to be
/// temporary would never be deleted, or one meant to be kept would not be kept.
fn check_temporary(board: &Board, wanted: bool, done: &str) -> Result<()> {
    if board.temporary != wanted {
        bail!(
            "{done} board {slug}, but the server ignored temporary={wanted}: it predates \
             temporary boards. Upgrade the server, then run board board update {slug} \
             --temporary {wanted}",
            slug = board.slug
        );
    }
    Ok(())
}

pub async fn board_delete(ctx: &Ctx, slug: &BoardSlug) -> Result<Output> {
    ctx.client.delete_board(slug).await?;
    Ok(Output::new(
        format!("deleted {slug}"),
        &serde_json::json!({ "deleted": slug }),
    ))
}

/// `board board adopt`: asks this device's daemon to make `thread`, an existing T3 thread,
/// the board's coordinator, then switches autopilot on and holds the coordinator lease for
/// this device. The adoption is saved before autopilot goes on, so this device's daemon
/// never starts a new thread for the board instead.
pub async fn board_adopt(
    ctx: &Ctx,
    t3: &impl T3,
    slug: &BoardSlug,
    thread: &str,
    now: Timestamp,
) -> Result<Output> {
    let agent = daemon::thread_agent(thread)?;
    let board = ctx.client.board(slug).await?;
    if board.status != BoardStatus::Active {
        bail!(
            "board {slug} is {}: only active boards run a coordinator; set it active first \
             (board board update {slug} --status active)",
            render::enum_str(&board.status)
        );
    }
    let device = &ctx.device;
    if !board.home_devices.contains(device) {
        bail!(
            "this device ({device}) is not a home device of board {slug}, so its daemon cannot \
             run the coordinator; run this on the device that has the thread, after adding it \
             (board board update {slug} --home-device {device}, repeating --home-device for \
             each device to keep)"
        );
    }
    if !board
        .repos
        .iter()
        .any(|repo| ctx.config.projects.contains_key(repo))
    {
        bail!(
            "config.toml has no [projects] path for any repo of board {slug} ({}); add one, \
             since the daemon registers the coordinator in that project",
            board.repos.join(", ")
        );
    }
    if let Some(current) = &board.coordinator {
        if *current == agent {
            // Nothing left to adopt: an older request must not replace this thread later.
            daemon::remove_adoption(&ctx.home, slug)?;
            return Ok(Output::new(
                format!(
                    "{agent} already coordinates {slug}\n{}",
                    render::board_line(&board)
                ),
                &board,
            ));
        }
        // A coordinator that has ended is replaced by the daemon, as when none is named.
        let live = match ctx.client.agent_info(current).await {
            Ok(info) => info.state != AgentState::Ended,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => false,
            Err(err) => return Err(err),
        };
        if live {
            bail!(
                "{current} coordinates board {slug} now; switch autopilot off (board board \
                 update {slug} --autopilot false), wait until `board boards` shows it without \
                 a coordinator, then adopt again"
            );
        }
    }
    if let Some(local) = daemon::load_watch_state(&ctx.home).coordinators.get(slug)
        && local.thread != thread
        && t3
            .thread_info(&local.thread)
            .await?
            .is_some_and(|t| !t.archived)
    {
        bail!(
            "the daemon on this device still keeps thread {} as the coordinator of board \
             {slug}; switch autopilot off (board board update {slug} --autopilot false) and \
             let the daemon run (board watch --once), then adopt again",
            local.thread
        );
    }
    match t3.thread_info(thread).await? {
        None => bail!(
            "T3 on this device knows no thread {thread}; pass the id of a thread on this device"
        ),
        Some(info) if info.archived => {
            bail!("thread {thread} is archived; unarchive it in T3 or adopt another thread")
        }
        Some(_) => {}
    }
    daemon::save_adoption(
        &ctx.home,
        slug,
        &Adoption {
            thread: thread.to_owned(),
            requested_at: now,
        },
    )?;
    if !board.autopilot {
        let body = UpdateBoard {
            autopilot: Some(true),
            ..UpdateBoard::default()
        };
        if let Err(err) = ctx.client.update_board(slug, &body).await {
            daemon::remove_adoption(&ctx.home, slug)?;
            return Err(err);
        }
    }
    // The lease, held for this device, keeps every other home device from starting a thread
    // for the board before this device's daemon adopts.
    let claimed = ctx
        .client
        .with_agent(None)
        .claim_coordinator(
            slug,
            &ClaimCoordinator {
                device: device.clone(),
                agent: None,
            },
        )
        .await;
    let board = match claimed {
        Ok(board) => board,
        Err(err) if error_code(&err) == Some(ErrorCode::ClaimRefused) => {
            daemon::remove_adoption(&ctx.home, slug)?;
            bail!(
                "another device runs the coordinator of board {slug} ({err:#}); switch autopilot \
                 off (board board update {slug} --autopilot false), wait until `board boards` \
                 shows no coordinator, then adopt again"
            );
        }
        // The adoption stays: the daemon acts on it without the lease being held now.
        Err(err) => return Err(err.context("reserving the coordinator lease for this device")),
    };
    Ok(Output::new(
        format!(
            "adopting {agent} as the coordinator of {slug}: the daemon on {device} (board watch) \
             briefs the thread and names it coordinator within a minute\n{}",
            render::board_line(&board)
        ),
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

/// A `--since` argument: an RFC 3339 time, a date (`YYYY-MM-DD`, midnight UTC), or a
/// duration back from `now` (`30m`, `12h`, `14d`).
pub fn parse_since(value: &str, now: Timestamp) -> Result<Timestamp> {
    let value = value.trim();
    let back = value
        .strip_suffix(['m', 'h', 'd'])
        .and_then(|n| n.parse::<u32>().ok())
        .map(|n| match value.as_bytes()[value.len() - 1] {
            b'm' => chrono::TimeDelta::minutes(n.into()),
            b'h' => chrono::TimeDelta::hours(n.into()),
            _ => chrono::TimeDelta::days(n.into()),
        });
    if let Some(back) = back {
        return now.checked_sub_signed(back).with_context(|| {
            format!("since {value:?} reaches back too far; send a shorter duration")
        });
    }
    match parse_not_before(Some(value)) {
        Ok(Some(Some(at))) => Ok(at),
        _ => bail!(
            "since {value:?} is not a time; send an RFC 3339 time, a date (YYYY-MM-DD) or a duration back from now (30m, 12h, 14d)"
        ),
    }
}

/// API errors the server recorded since a time, one line per signature (`GET /v1/errors`).
pub async fn errors(ctx: &Ctx, q: &ErrorsQuery) -> Result<Output> {
    let log = ctx.client.errors(q).await?;
    Ok(Output::new(render::errors(&log), &log))
}

pub async fn stats(ctx: &Ctx, days: u32) -> Result<Output> {
    let stats = ctx.client.stats(days).await?;
    Ok(Output::new(render::stats(&stats), &stats))
}

/// Reports something in a setup that cost a step (`POST /v1/feedback`): the server files a
/// feedback item, or adds the report to the live item with the same signature.
pub async fn feedback(ctx: &Ctx, req: CreateFeedback) -> Result<Output> {
    if req.text.trim().is_empty() {
        bail!("feedback is empty: say what cost you a step");
    }
    let result = ctx.client.feedback(&req).await?;
    let task = &result.task;
    let text = if result.duplicate {
        format!(
            "added your report to {} ({} reports): {}",
            task.id,
            result.reports,
            render::task_line(task)
        )
    } else {
        format!("filed {}", render::task_line(task))
    };
    Ok(Output::new(
        format!("{text}\nthanks; carry on with your work"),
        &result,
    ))
}

/// Resolves a feedback item you own (or as the user): it moves to done.
pub async fn resolve(ctx: &Ctx, id: &TaskId, req: ResolveFeedback) -> Result<Output> {
    let task = ctx.client.resolve_feedback(id, &req).await?;
    Ok(Output::new(render::task_line(&task), &task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_time_a_date_or_a_duration_back() {
        let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
        let at = |s: &str| s.parse::<Timestamp>().unwrap();
        for (since, want) in [
            ("30m", "2026-11-08T11:30:00Z"),
            ("12h", "2026-11-08T00:00:00Z"),
            ("14d", "2026-10-25T12:00:00Z"),
            ("2026-11-07", "2026-11-07T00:00:00Z"),
            ("2026-11-07T07:45:00Z", "2026-11-07T07:45:00Z"),
        ] {
            assert_eq!(parse_since(since, now).unwrap(), at(want), "{since}");
        }
        for bad in ["", "d", "14w", "-3d", "none", "yesterday", "100000000d"] {
            assert!(parse_since(bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn enum_values_are_the_wire_spellings() {
        assert_eq!(enum_values::<Priority>(), ["p1", "p2", "p3"]);
        assert_eq!(
            enum_values::<NoteKind>(),
            ["note", "finding", "decision", "feedback"]
        );
        assert_eq!(enum_values::<String>(), [] as [&str; 0]);
        assert_eq!(
            parse_enum::<Priority>("1").unwrap_err(),
            "unknown value \"1\"; send one of p1, p2, p3"
        );
        assert_eq!(parse_enum::<Priority>("p1"), Ok(Priority::P1));
    }

    #[test]
    fn over_long_text_is_refused_before_sending() {
        assert!(check_chars("title", &"x".repeat(72), limits().title_chars).is_ok());
        let err = check_chars("title", &"x".repeat(80), limits().title_chars).unwrap_err();
        assert_eq!(
            err.to_string(),
            "title is 80 characters; the limit is 72 (not sent)\n\
             next: shorten title to at most 72 characters, or put the detail in the body"
        );
    }
}
