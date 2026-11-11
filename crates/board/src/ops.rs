//! Board operations shared by the CLI and the MCP server: each returns concise text and
//! the JSON the server answered.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use board_proto::{
    AddNote, AgentId, AgentRole, AgentSource, AgentState, AskKind, Board, BoardSlug, BoardStatus,
    ClaimCoordinator, CreateAsk, CreateBoard, CreateFeedback, CreateLeases, CreateTask, DeviceName,
    DoneTask, ErrorCode, EvidenceKind, Lease, NewEvidence, NoteKind, Priority, PutHandoff,
    RegisterAgent, ResolveFeedback, Runtime, SnapshotFormat, SnapshotKind, TASKS_LIMIT_DEFAULT,
    TASKS_LIMIT_MAX, Task, TaskDetail, TaskId, TaskStatus, Timestamp, UpdateBoard, UpdateTask,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::client::{Client, ErrorsQuery, SnapshotQuery, TaskQuery, error_code};
use crate::config::{Config, Home, load_token};
use crate::daemon::{self, Adoption};
use crate::identity::RepoInfo;
use crate::render;
use crate::state::{Filed, FiledMark};
use crate::t3::T3;

/// How an agent other than the environment's own was named, which decides how the
/// refusal of an unknown one reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedAgent {
    /// `--agent` on a CLI command.
    CliFlag,
    /// `--agent` given to `board mcp` at start-up.
    McpStart,
    /// `agent=` in an MCP tool call.
    McpArg,
}

impl NamedAgent {
    fn unknown(self, agent: &AgentId) -> anyhow::Error {
        let what = "which the server does not know, so nothing was done";
        let hooks = "A new session registers through its hooks or the daemon";
        match self {
            Self::CliFlag => anyhow::anyhow!(
                "--agent names {agent}, {what}: drop --agent to act as yourself. {hooks}; a \
                 script that acts under its own fixed id passes --register with --agent."
            ),
            Self::McpStart => anyhow::anyhow!(
                "this MCP server was started with --agent {agent}, {what}. Ask the user to \
                 start it without --agent, so it acts as the session, or with --register \
                 for a script's own fixed id."
            ),
            Self::McpArg => anyhow::anyhow!(
                "agent= names {agent}, {what}: leave agent out to act as yourself (to give \
                 another agent a task, hand it over with owner=). {hooks}."
            ),
        }
    }
}

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
    /// An agent named by hand (`named`) is refused instead when unknown, before anything is
    /// written, since such an id would stay listed as running with nothing to end it.
    pub async fn ensure_registered(&self, named: Option<NamedAgent>) -> Result<()> {
        let Some(agent) = self.client.agent() else {
            return Ok(());
        };
        match self.client.agent_info(agent).await {
            Ok(_) => Ok(()),
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                if let Some(named) = named {
                    return Err(named.unknown(agent));
                }
                let runtime = match agent.source() {
                    AgentSource::Codex => Runtime::Codex,
                    AgentSource::Opencode => Runtime::Opencode,
                    // A dispatch agent runs no model: it names the runtime of the threads
                    // it starts, Claude.
                    AgentSource::T3 | AgentSource::Claude | AgentSource::Dispatch => {
                        Runtime::Claude
                    }
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

/// Text over a limit, refused before the request was sent, with the server's message and
/// hint. The CLI reports it to the server (`POST /v1/errors`), so signals count it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The message the server would have answered.
    pub message: String,
    pub next: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (not sent)\nnext: {}", self.message, self.next)
    }
}

impl std::error::Error for Refused {}

/// Refuses `value` when it has more than `max` characters, with the server's message and
/// hint, so over-long text fails before a request is sent.
pub fn check_chars(field: &str, value: &str, max: usize) -> Result<()> {
    check_chars_or(field, value, max, board_proto::longer_text_goes(field))
}

/// [`check_chars`] with `goes` saying where longer text goes, as the server says it.
pub fn check_chars_or(field: &str, value: &str, max: usize, goes: Option<&str>) -> Result<()> {
    match board_proto::TooLong::check(field, value, max) {
        Some(long) => Err(Refused {
            message: long.message(),
            next: long.next_or(goes),
        }
        .into()),
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

/// Agents `board status` lists unless `detailed`: every live one, then the ended ones seen
/// last, up to this many together.
pub const STATUS_AGENTS: usize = 20;
/// Asks `board asks` lists when the call names no limit and the asks are not the open ones:
/// the newest.
pub const ASKS_LIMIT_DEFAULT: usize = 50;

/// Without `detailed`, leaves out the ended agents past [`STATUS_AGENTS`] agents, the ones
/// seen longest ago, and says how many: a board keeps every agent of the last 7 days, most
/// of them ended child threads. An agent that is not ended may own work, so it always shows.
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
            let mut agents = ctx.client.agents(Some(&slug)).await?;
            let mut more = 0;
            if !detailed {
                let mut ended: Vec<Timestamp> = agents
                    .iter()
                    .filter(|a| a.state == AgentState::Ended)
                    .map(|a| a.last_seen)
                    .collect();
                let keep = STATUS_AGENTS.saturating_sub(agents.len() - ended.len());
                more = ended.len().saturating_sub(keep);
                if more > 0 {
                    ended.sort_unstable_by(|a, b| b.cmp(a));
                    // The server's order (by handle) stays; ties at the cut go by that order.
                    let oldest_kept = keep.checked_sub(1).map(|i| ended[i]);
                    let mut left = keep;
                    agents.retain(|a| {
                        if a.state != AgentState::Ended {
                            return true;
                        }
                        let kept = left > 0 && oldest_kept.is_some_and(|at| a.last_seen >= at);
                        left -= usize::from(kept);
                        kept
                    });
                }
            }
            lines.push(render::board_detail(&b, &agents, detailed));
            let mut json = json!({"board": b, "agents": agents});
            if more > 0 {
                lines.push(format!(
                    "left out {more} ended agents, seen before the ones listed: detailed lists every one"
                ));
                json["agents_left_out"] = json!(more);
            }
            json
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

/// `fields`, when given, are the only fields of each task in the JSON output; one a task
/// does not have is null.
pub async fn task_list(
    ctx: &Ctx,
    query: &TaskQuery,
    detailed: bool,
    fields: Option<&[String]>,
) -> Result<Output> {
    if let Some(unknown) = fields
        .into_iter()
        .flatten()
        .find(|f| !TASK_FIELDS.contains(&f.as_str()))
    {
        bail!(
            "--fields: a task has no field {unknown:?}; its fields are {}",
            TASK_FIELDS.join(", ")
        );
    }
    // The server cuts a list at its limit without saying so: ask for one task more, so
    // the text can say that the list was cut. At the server's maximum that is a guess.
    let limit = query
        .limit
        .unwrap_or(TASKS_LIMIT_DEFAULT)
        .clamp(1, TASKS_LIMIT_MAX);
    let probe = TaskQuery {
        limit: Some((limit + 1).min(TASKS_LIMIT_MAX)),
        ..query.clone()
    };
    let mut tasks = ctx.client.tasks(&probe).await?;
    let limit = limit as usize;
    let notice = if tasks.len() > limit {
        tasks.truncate(limit);
        // The last task listed is the cursor of the next page.
        let last = &tasks[limit - 1].id;
        // A page after a cursor is not the first of the list.
        let which = if query.after.is_some() {
            "next"
        } else {
            "first"
        };
        format!(
            "\nlisted the {which} {limit}; more match: list the next with after {last}, narrow it with a filter or a search, or raise the limit (at most {TASKS_LIMIT_MAX})"
        )
    } else if let Some(last) = tasks
        .last()
        .filter(|_| tasks.len() == TASKS_LIMIT_MAX as usize)
    {
        format!(
            "\nlisted {TASKS_LIMIT_MAX}, the most one call lists; more may match: list the next with after {}, or narrow it with a filter or a search",
            last.id
        )
    } else {
        String::new()
    };
    // The server marks each task of a search that found no task with every word.
    let notice = if tasks.iter().any(|t| t.any_word) {
        format!("\nno task holds every word of the search; these hold at least one{notice}")
    } else {
        notice
    };
    // A list across boards says which ones it holds; a parent's children are on its board.
    let notice = if query.board.is_none() && query.parent.is_none() && !tasks.is_empty() {
        let boards: BTreeSet<&str> = tasks.iter().map(|t| t.board.as_str()).collect();
        let boards: Vec<&str> = boards.into_iter().collect();
        format!(
            "{notice}\nboards listed: {}; name a board to narrow it",
            boards.join(", ")
        )
    } else if tasks.is_empty() && query.closed == Some(false) {
        // An empty list says what it left out, so nobody reads it as an empty board.
        let board = query
            .board
            .as_ref()
            .map(|b| format!(" on board {b}"))
            .unwrap_or_default();
        format!(
            "{notice}
looked{board} at open tasks only: done and dropped ones are left out"
        )
    } else {
        notice
    };
    let text = if detailed {
        render::task_list_detailed(&tasks, &notice)
    } else {
        render::task_list(&tasks) + &notice
    };
    let Some(fields) = fields else {
        return Ok(Output::new(text, &tasks));
    };
    let picked: Vec<Value> = tasks
        .iter()
        .map(|task| {
            let whole = serde_json::to_value(task)?;
            Ok(fields
                .iter()
                .map(|f| (f.clone(), whole.get(f).cloned().unwrap_or(Value::Null)))
                .collect::<serde_json::Map<_, _>>()
                .into())
        })
        .collect::<Result<_>>()?;
    Ok(Output::new(text, &picked))
}

/// The fields of a task in JSON (`board_proto::Task`), which `board ls --fields` picks from.
const TASK_FIELDS: [&str; 26] = [
    "id",
    "board",
    "parent",
    "title",
    "body",
    "acceptance",
    "status",
    "priority",
    "blocked_reason",
    "owner",
    "owner_handle",
    "owner_stale",
    "blockers",
    "ready",
    "proposal",
    "not_before",
    "auto",
    "created_by",
    "version",
    "created_at",
    "updated_at",
    "feedback",
    "summary",
    "child_count",
    "snippet",
    "any_word",
];

pub async fn task_show(
    ctx: &Ctx,
    id: &TaskId,
    detailed: bool,
    since: Option<render::Since>,
) -> Result<Output> {
    let detail = ctx.client.task(id).await?;
    let text = match since {
        Some(since) => render::task_changes(&detail, since),
        None => render::task_detail(&detail, detailed),
    };
    Ok(Output::new(text, &detail))
}

/// The `since` of a repeat `task_show`: an event sequence number, as the last `task_show`
/// named it, or a time, date or duration back from `now` ([`parse_since`]).
pub fn show_since(value: &str, now: Timestamp) -> Result<render::Since> {
    let value = value.trim();
    if let Ok(seq) = value.parse::<i64>()
        && seq >= 0
    {
        return Ok(render::Since::Seq(seq));
    }
    parse_since(value, now)
        .map(render::Since::At)
        .map_err(|e| anyhow!("{e:#}; or send the number the last task_show ended with"))
}

/// Most tasks one `task_show` reads.
pub const TASK_SHOW_MAX: usize = 20;

/// One task, or several in one call. Several come in a compact form: each task's state,
/// evidence, text, findings and decisions. They are read at the same time; one that cannot
/// be read is named with the reason and does not fail the others. An id named twice is
/// read once, so the same id alone is the full form.
///
/// `since` shows one task's changes after that point instead ([`render::task_changes`]).
pub async fn tasks_show(
    ctx: &Ctx,
    ids: &[TaskId],
    detailed: bool,
    since: Option<render::Since>,
) -> Result<Output> {
    let mut unique: Vec<&TaskId> = Vec::with_capacity(ids.len());
    for id in ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    if detailed && since.is_some() {
        bail!("since shows the changes only: drop detailed, or drop since for the whole task");
    }
    if let [id] = unique.as_slice() {
        return task_show(ctx, id, detailed, since).await;
    }
    if since.is_some() {
        bail!("since shows one task's changes: name one id, or drop since");
    }
    if detailed {
        bail!("detailed shows one task: name one id, or drop detailed");
    }
    if unique.len() > TASK_SHOW_MAX {
        bail!(
            "{} tasks is more than one call shows; name at most {TASK_SHOW_MAX}",
            unique.len()
        );
    }
    let read = futures_util::future::join_all(unique.iter().map(|id| ctx.client.task(id))).await;
    let tasks: Vec<(TaskId, Result<TaskDetail, String>)> = unique
        .into_iter()
        .cloned()
        .zip(read.into_iter().map(|r| r.map_err(|e| format!("{e:#}"))))
        .collect();
    if let Some((_, Err(reason))) = tasks.first()
        && tasks.iter().all(|(_, read)| read.is_err())
    {
        bail!("none of the tasks could be read: {reason}");
    }
    let details: Vec<&TaskDetail> = tasks.iter().filter_map(|(_, r)| r.as_ref().ok()).collect();
    let not_shown: Vec<serde_json::Value> = tasks
        .iter()
        .filter_map(|(id, r)| Some(serde_json::json!({ "id": id, "reason": r.as_ref().err()? })))
        .collect();
    Ok(Output::new(
        render::task_details(&tasks),
        &serde_json::json!({ "tasks": details, "not_shown": not_shown }),
    ))
}

/// A `since` filter of the task list as the server takes it: an event sequence number as
/// it is, and a time, date or duration back from `now` ([`parse_since`]) as a time.
pub fn list_since(value: &str, now: Timestamp) -> Result<String> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(value.to_owned());
    }
    parse_since(value, now)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .map_err(|e| anyhow!("{e:#}; or send an event sequence number, such as the snapshot's seq"))
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
    // For the Stop hook, which does not remind an agent that filed follow-ups to file them
    // (`hook::done_reminder`). The task exists either way, so a failed write costs only that.
    let kind = if task.is_pending_proposal() {
        Some(Filed::Proposal)
    } else {
        task.parent.as_ref().map(|_| Filed::SubTask)
    };
    if let Some(kind) = kind {
        record_filed(ctx, task.id.as_str(), kind, task.parent.clone());
    }
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
    if let Some(wanted) = &wanted_owner {
        check_owner(ctx, &task, wanted).await?;
    }
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
    take: bool,
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
                take,
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

/// Leaves a mark of what the agent filed for its next `Stop` hook; see [`task_add`].
fn record_filed(ctx: &Ctx, name: &str, kind: Filed, about: Option<TaskId>) {
    let mark = FiledMark {
        kind,
        about,
        at: chrono::Utc::now(),
    };
    let recorded = ctx
        .agent()
        .and_then(|agent| crate::state::record_filed(&ctx.home, agent, name, &mark));
    if let Err(err) = recorded {
        tracing::warn!("recording {name} filed for the Stop hook: {err:#}");
    }
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
    let holders = lease_holders(&ctx.client, &result.conflicts).await;
    Ok(Output::new(
        render::lease_result(&result, &holders),
        &result,
    ))
}

/// Looks up the status of each overlapping lease's task and its holder's short handle. Best
/// effort: a failed lookup leaves that entry out, and the warning prints the bare id.
async fn lease_holders(client: &Client, conflicts: &[Lease]) -> render::LeaseHolders {
    let tasks: BTreeSet<&TaskId> = conflicts.iter().filter_map(|l| l.task.as_ref()).collect();
    let agents: BTreeSet<&AgentId> = conflicts.iter().map(|l| &l.agent).collect();
    let (statuses, handles) = futures_util::future::join(
        futures_util::future::join_all(tasks.into_iter().map(|id| async move {
            let detail = client.task(id).await.ok()?;
            Some((id.clone(), detail.task.status))
        })),
        futures_util::future::join_all(agents.into_iter().map(|id| async move {
            let agent = client.agent_info(id).await.ok()?;
            Some((id.to_string(), agent.handle))
        })),
    )
    .await;
    render::LeaseHolders {
        statuses: statuses.into_iter().flatten().collect(),
        handles: handles.into_iter().flatten().collect(),
    }
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
    record_filed(
        ctx,
        &format!("ask-{}", created.id),
        Filed::Ask,
        created.task.clone(),
    );
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
        match ctx
            .client
            .asks(Some("open"), Some(&created.board), None)
            .await
        {
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

/// Lists the newest `limit` asks that match, oldest first, and says how many it left out.
pub async fn asks(
    ctx: &Ctx,
    status: Option<&str>,
    board: Option<&BoardSlug>,
    limit: Option<usize>,
) -> Result<Output> {
    // Every open ask waits on the user, the oldest the longest, so none is left out unless
    // the caller asks. The answered and withdrawn ones grow without end.
    let limit = limit.or((status != Some("open")).then_some(ASKS_LIMIT_DEFAULT));
    // The server cuts the list at its limit without saying so: ask for one ask more, so
    // the text can say that the list was cut. The server reads the limit as a u32.
    let probe = limit.map(|limit| limit.saturating_add(1).min(u32::MAX as usize));
    let mut asks = ctx.client.asks(status, board, probe).await?;
    // Newest last, whatever order the server sends.
    asks.sort_by_key(|a| a.id);
    let more = limit.map_or(0, |limit| asks.len().saturating_sub(limit));
    asks.drain(..more);
    let mut text = render::ask_list(&asks);
    if let Some(limit) = limit
        && more > 0
    {
        // One ask over the limit is all a server that cuts the list sends, so it gives no
        // count; one that predates the limit sends every ask, which gives the count when
        // more than one is over.
        let older = if more == 1 {
            "older ones match".to_owned()
        } else {
            format!("{more} older match")
        };
        text.push_str(&format!(
            "\nlisted the newest {limit}; {older}: narrow it with --status or --board, or raise --limit"
        ));
    }
    Ok(Output::new(text, &asks))
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

/// `board usage`: the latest reading, and which boards it holds from new work.
pub async fn usage(ctx: &Ctx, reserve: Option<Option<f64>>, now: Timestamp) -> Result<Output> {
    let mut set = None;
    if let Some(reserve) = reserve {
        set = Some(
            ctx.client
                .set_usage_reserve(&board_proto::UsageReserve { reserve })
                .await?,
        );
    }
    let usage = ctx.client.usage().await?;
    let boards = match &usage {
        Some(_) => ctx.client.boards().await?,
        None => Vec::new(),
    };
    // A reading without a pace may still have a reserve, waiting for a weekly reset time.
    let reserve = match (set, usage.as_ref().and_then(|u| u.pace)) {
        (Some(set), _) => set.reserve,
        (None, Some(p)) => Some(p.reserve),
        (None, None) => match ctx.client.usage_reserve().await {
            Ok(r) => r.reserve,
            Err(err) => {
                // A server that predates the reserve: render it as off.
                tracing::debug!("reading the usage reserve: {err:#}");
                None
            }
        },
    };
    Ok(Output::new(
        render::usage(usage.as_ref(), reserve, &boards, now),
        &usage,
    ))
}

/// `board pause` and `board resume`: sets the boards `named`, or by default every board in
/// status `from`, to status `to`, and lists the boards it changed. Named boards are checked
/// before any changes, so a wrong name changes nothing. The server never pauses its feedback
/// board; a pause lists it apart, as kept active.
pub async fn set_boards_status(
    ctx: &Ctx,
    named: Vec<BoardSlug>,
    from: BoardStatus,
    to: BoardStatus,
) -> Result<Output> {
    let boards = ctx.client.boards().await?;
    let targets: Vec<&Board> = if named.is_empty() {
        boards.iter().filter(|b| b.status == from).collect()
    } else {
        let mut targets = Vec::new();
        for slug in &named {
            let board = boards
                .iter()
                .find(|b| &b.slug == slug)
                .ok_or_else(|| anyhow!("no board {slug}: board boards lists them"))?;
            if board.status == BoardStatus::Closed {
                bail!(
                    "board {slug} is closed: reopen it with board board update {slug} --status \
                     active, or leave it out"
                );
            }
            if !targets.iter().any(|t: &&Board| t.slug == board.slug) {
                targets.push(board);
            }
        }
        targets
    };
    let verb = match to {
        BoardStatus::Paused => "paused",
        _ => "resumed",
    };
    let mut changed: Vec<Board> = Vec::new();
    let mut kept: Vec<Board> = Vec::new();
    for board in targets.into_iter().filter(|b| b.status != to) {
        let body = UpdateBoard {
            status: Some(to),
            ..UpdateBoard::default()
        };
        let updated = ctx
            .client
            .update_board(&board.slug, &body)
            .await
            .with_context(|| {
                let done: Vec<String> = changed.iter().map(|b| b.slug.to_string()).collect();
                format!(
                    "setting board {} to {verb} ({verb} so far: {})",
                    board.slug,
                    if done.is_empty() {
                        "none".to_owned()
                    } else {
                        done.join(", ")
                    }
                )
            })?;
        if updated.status == to {
            changed.push(updated);
        } else {
            kept.push(updated);
        }
    }
    let mut text = if changed.is_empty() && kept.is_empty() {
        format!("no boards {verb}: none was {}", render::enum_str(&from))
    } else if changed.is_empty() {
        format!("no boards {verb}")
    } else {
        let plural = if changed.len() == 1 { "" } else { "s" };
        let mut text = format!("{verb} {} board{plural}:", changed.len());
        for board in &changed {
            text.push_str("\n  ");
            text.push_str(&render::board_line(board));
        }
        text
    };
    for board in &kept {
        text.push_str(&format!(
            "\n{} stays {}: {}",
            board.slug,
            render::enum_str(&board.status),
            KEPT_ACTIVE
        ));
    }
    Ok(Output::new(text, &changed))
}

/// Why a board the server was asked to pause stays active.
const KEPT_ACTIVE: &str = "the server never pauses its feedback board, so triage goes on";

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
    if let Some(wanted) = body.parallel_tasks
        && board.parallel_tasks != wanted
    {
        bail!(
            "updated board {slug}, but the server ignored parallel_tasks={wanted}: it predates \
             parallel tasks. Upgrade the server, then run board board update {slug} \
             --parallel-tasks {wanted}"
        );
    }
    if let Some(wanted) = body.usage_stop_at
        && board.usage_stop_at != wanted
    {
        bail!(
            "updated board {slug}, but the server ignored usage_stop_at={wanted}: it predates \
             usage awareness. Upgrade the server, then run board board update {slug} \
             --usage-stop-at {wanted}"
        );
    }
    if let Some(wanted) = body.dispatch
        && board.dispatch != wanted
    {
        let wanted = crate::render::enum_str(&wanted);
        bail!(
            "updated board {slug}, but the server ignored dispatch={wanted}: it predates \
             daemon dispatch. Upgrade the server, then run board board set {slug} \
             --dispatch {wanted}"
        );
    }
    let mut text = format!("updated {}", render::board_line(&board));
    if body.status == Some(BoardStatus::Paused) && board.status != BoardStatus::Paused {
        text.push_str(&format!(
            "\n{slug} stays {}: {KEPT_ACTIVE}",
            render::enum_str(&board.status)
        ));
    }
    Ok(Output::new(text, &board))
}

/// A server that predates handovers ignores `owner` and answers with the owner unchanged,
/// so the new owner's `task_done` would be refused later. The server resolves a short
/// handle such as `t3:7f3a` to the agent's full id, so a different id in the answer is
/// checked against the handles the task's detail lists before it counts as ignored.
async fn check_owner(ctx: &Ctx, task: &Task, wanted: &AgentId) -> Result<()> {
    let Some(owner) = task.owner.as_ref() else {
        return Err(ignored_owner(task, wanted));
    };
    if owner == wanted {
        return Ok(());
    }
    let detail = ctx.client.task(&task.id).await?;
    if detail.task.owner.as_ref() == Some(owner)
        && detail.handles.get(owner.as_str()).map(String::as_str) == Some(wanted.as_str())
    {
        return Ok(());
    }
    Err(ignored_owner(task, wanted))
}

fn ignored_owner(task: &Task, wanted: &AgentId) -> anyhow::Error {
    anyhow!(
        "updated {id}, but the server ignored owner={wanted}: it predates handovers, so \
         {owner} still owns it. Upgrade the server, then hand it over again",
        id = task.id,
        owner = task.owner.as_ref().map_or("nobody", AgentId::as_str),
    )
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
    // The daemon's own lookup ([projects] for a board repo, else [board_projects] for the
    // board), so adopt accepts exactly the folders the daemon runs the thread in; the
    // daemon runs it only while the folder is there.
    let project = daemon::host_folder(&ctx.config, &board)?;
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
        let info = match ctx.client.agent_info(current).await {
            Ok(info) => Some(info),
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => None,
            Err(err) => return Err(err),
        };
        match info {
            Some(info) if info.state != AgentState::Ended => bail!(
                "{current} coordinates board {slug} now; switch autopilot off (board board \
                 update {slug} --autopilot false), wait until `board boards` shows it without \
                 a coordinator, then adopt again"
            ),
            // T3 ends a session left idle while its thread lives on, and that device's
            // daemon keeps the lease while the thread is live. On this device, the adoption
            // replaces it, as when none is named.
            Some(info) if info.device != *device => bail!(
                "{current} coordinates board {slug} from device {}: its session ended, but \
                 that device keeps the coordinator lease while its thread lives; adopt on that \
                 device, or wait until `board boards` shows {slug} without a coordinator (the \
                 lease expires once that device stops renewing it: after the server's \
                 coordinator_ttl_secs, 45 minutes by default), then adopt again",
                info.device
            ),
            _ => {}
        }
    }
    if let Some(local) = daemon::load_watch_state(&ctx.home).coordinators.get(slug)
        && local.thread != thread
        && t3
            .thread_info(&local.thread)
            .await?
            .is_some_and(|t| t.runnable(Some(&project)))
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
        Some(info) if !info.runnable(Some(&project)) => bail!(
            "the worktree {} of thread {thread} is gone, so T3 fails every run of it; adopt \
             another thread",
            info.folder(Some(&project)).unwrap_or(&project).display()
        ),
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

pub async fn stats(ctx: &Ctx, days: u32, now: Timestamp) -> Result<Output> {
    let stats = ctx.client.stats(days).await?;
    Ok(Output::new(render::stats(&stats, now), &stats))
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

/// Resolves a feedback item you own or an open one nobody owns (or as the user): it moves to done.
pub async fn resolve(ctx: &Ctx, id: &TaskId, req: ResolveFeedback) -> Result<Output> {
    let task = ctx.client.resolve_feedback(id, &req).await?;
    Ok(Output::new(render::task_line(&task), &task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_since_takes_an_event_number_or_a_time() {
        let now: Timestamp = "2026-11-07T10:00:00Z".parse().unwrap();
        assert_eq!(
            show_since(" 33990 ", now).unwrap(),
            render::Since::Seq(33990)
        );
        assert_eq!(
            show_since("30m", now).unwrap(),
            render::Since::At("2026-11-07T09:30:00Z".parse().unwrap())
        );
        let err = show_since("latest", now).unwrap_err().to_string();
        assert!(
            err.contains("the number the last task_show ended with"),
            "{err}"
        );
    }

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
        assert!(check_chars("title", &"x".repeat(80), limits().title_chars).is_ok());
        let err = check_chars("title", &"x".repeat(81), limits().title_chars).unwrap_err();
        assert_eq!(
            err.to_string(),
            "title is 81 characters; the limit is 80 (not sent)\n\
             next: shorten title to at most 80 characters, or put the detail in the body"
        );
    }
}
