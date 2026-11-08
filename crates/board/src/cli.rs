//! Command-line interface.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use board_proto::{
    ASK_OPTION_CHARS_MAX, AskKind, BoardSlug, BoardStatus, CreateBoard, CreateFeedback, DeviceName,
    EVIDENCE_VALUE_CHARS_MAX, FEEDBACK_ABOUT_MAX, FEEDBACK_SIGNATURE_MAX, FeedbackActivity,
    FeedbackSource, NoteKind, Priority, RESOLUTION_REASON_CHARS_MAX, ResolutionKind,
    ResolveFeedback, SUMMARY_CHARS_MAX, SnapshotFormat, SnapshotKind, TaskId, TaskStatus,
    UpdateBoard,
};
use clap::builder::TypedValueParser as _;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::client::TaskQuery;
use crate::config::{Config, Home, TokenStore, store_token};
use crate::identity::{agent_from_env, proven_identity};
use crate::ops::{self, Ctx, Output, check_chars, limits, parse_enum};

#[derive(Debug, Parser)]
#[command(
    name = "board",
    version = crate::BUILD,
    about = "Shared work board for coding-agent threads"
)]
pub struct Cli {
    /// Act as this agent id instead of the one from the environment. In a T3 thread, only
    /// the thread's own id is accepted. An id the server does not know is refused, unless
    /// --register is given too.
    #[arg(long, global = true)]
    pub agent: Option<String>,
    /// Register the --agent id when the server does not know it yet: for a script that
    /// acts under its own fixed id. Sessions register through their hooks instead.
    #[arg(long, global = true, requires = "agent")]
    pub register: bool,
    /// Print JSON instead of text. Each command's --help names the shape it prints.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

/// What `--json` prints at the top level, per command path, for its `--help`. Every
/// command has an entry: a CLI test walks the command tree.
const JSON_SHAPES: &[(&str, &str)] = &[
    (
        "status",
        "{board, agents} for a board, or {boards: [...]} when none resolves",
    ),
    ("ls", "a JSON array of tasks"),
    (
        "show",
        "{task, notes, evidence, blocks, children, child_tasks?, history}",
    ),
    ("add", "the task object {id, status, owner, ...}"),
    ("claim", "the task object {id, status, owner, ...}"),
    ("release", "the task object {id, status, owner, ...}"),
    ("take", "the task object {id, status, owner, ...}"),
    ("update", "the task object {id, status, owner, ...}"),
    ("done", "the task object {id, status, owner, ...}"),
    ("note", "the note object {id, task, agent, kind, text, at}"),
    (
        "handoff",
        "the handoff object {agent, now, next, blockers, version, ...}, or null when there is none",
    ),
    ("lease", "{leases: [...], conflicts: [...], warning?}"),
    (
        "ask",
        "the ask object {id, status, question, ..., warning?}",
    ),
    ("asks", "a JSON array of asks"),
    ("withdraw", "the ask object {id, status, question, ...}"),
    ("answer", "the ask object {id, status, question, ...}"),
    ("join", "the board object {slug, title, status, ...}"),
    ("boards", "a JSON array of boards"),
    ("board", "see each subcommand's --help"),
    (
        "board create",
        "the board object {slug, title, status, ...}",
    ),
    (
        "board update",
        "the board object {slug, title, status, ...}",
    ),
    ("board delete", "{deleted: slug}"),
    ("board adopt", "the board object {slug, title, status, ...}"),
    ("log", "{events: [...], next_since}"),
    (
        "stats",
        "{days, tasks_created, tasks_done, ..., boards: [...]}",
    ),
    ("errors", "{signatures: [...], since, retention_days}"),
    ("snapshot", "{kind, text, seq, at}"),
    ("feedback", "{task, duplicate, reports}"),
    ("resolve", "the task object {id, status, owner, ...}"),
];

/// Commands that print no JSON and ignore `--json`, with their subcommands.
const JSON_IGNORED: &[&str] = &[
    "login",
    "mcp",
    "hook",
    "read-thread",
    "install",
    "watch",
    "serve",
    "admin",
];

/// The `--json` help for a command path, such as `board create`.
fn json_help(path: &str) -> Option<String> {
    if let Some((_, shape)) = JSON_SHAPES.iter().find(|(p, _)| *p == path) {
        return Some(format!("Print JSON instead of text: {shape}"));
    }
    let top = path.split(' ').next().unwrap_or(path);
    JSON_IGNORED
        .contains(&top)
        .then(|| "Ignored: this command prints no JSON".to_owned())
}

/// The command line, with each command's `--json` help naming the shape it prints.
pub fn command() -> clap::Command {
    use clap::CommandFactory as _;
    let mut cmd = Cli::command();
    // Copies the global --json into every subcommand, so each copy can get its own help.
    cmd.build();
    describe_json(cmd, "")
}

fn describe_json(mut cmd: clap::Command, path: &str) -> clap::Command {
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect();
    for name in names {
        let path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path} {name}")
        };
        cmd = cmd.mut_subcommand(&name, |sub| {
            let sub = match json_help(&path) {
                // Not `mut_arg`: it moves the arg to the end, which breaks the built key
                // index, so `--help` would parse as `--json`. `mut_args` keeps the order.
                Some(help) => sub.mut_args(|a| {
                    if a.get_id() == "json" {
                        a.help(help.clone())
                    } else {
                        a
                    }
                }),
                None => sub,
            };
            describe_json(sub, &path)
        });
    }
    cmd
}

/// Parses the process arguments like [`Parser::parse`], with [`command`]'s help.
pub fn parse() -> Cli {
    use clap::FromArgMatches as _;
    let mut matches = command().get_matches();
    Cli::from_arg_matches_mut(&mut matches).unwrap_or_else(|err| err.format(&mut command()).exit())
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Store the server URL and device name; reads the device token from stdin.
    Login {
        #[arg(long)]
        server: String,
        #[arg(long)]
        device: DeviceName,
    },
    /// Who you are, and where the board for this repo stands.
    Status {
        #[arg(long)]
        board: Option<BoardSlug>,
        #[arg(long)]
        detailed: bool,
    },
    /// List tasks.
    Ls(LsArgs),
    /// Show a task with its evidence, every finding and decision note, and the latest plain
    /// notes (--detailed for all and the history).
    Show {
        id: TaskId,
        #[arg(long)]
        detailed: bool,
    },
    /// Add a task.
    Add(AddArgs),
    /// Claim a task: it moves to doing with you as owner. Also resumes your own, or
    /// nobody's, task in review or blocked, unless a blocked reason or an open ask holds it.
    Claim { id: TaskId },
    /// Release a task you own back to open.
    Release { id: TaskId },
    /// Take a task whose owner is stale.
    Take { id: TaskId },
    /// Change a task's fields or status.
    Update(UpdateArgs),
    /// Mark a task done with evidence, only when every acceptance check passes; file what
    /// remains first (sub-tasks, proposals).
    Done(DoneArgs),
    /// Add a note to a task.
    Note {
        id: TaskId,
        #[arg(help = at_most(
            "The note: start with its kind and cite evidence. Split longer text across notes, \
             or put it in the task body or task_done evidence",
            limits().note_chars,
        ))]
        text: String,
        #[arg(long, default_value = "note", value_parser = enum_arg_hiding::<NoteKind>(&["feedback"]))]
        kind: NoteKind,
    },
    /// Show your handoff, or replace it with --now and --next.
    Handoff {
        #[arg(long, requires = "next", help = at_most("Where the work stands", limits().handoff_now_max()))]
        now: Option<String>,
        #[arg(long, requires = "now", help = at_most("The first concrete action", limits().handoff_field_chars))]
        next: Option<String>,
        #[arg(long, default_value = "", help = at_most("What blocks you, if anything", limits().handoff_field_chars))]
        blockers: String,
        /// Show another agent's handoff.
        #[arg(long, conflicts_with = "now")]
        of: Option<board_proto::AgentId>,
    },
    /// Lease files (globs relative to the repo root) to warn peers.
    Lease {
        #[arg(required = true)]
        globs: Vec<String>,
        /// The task the work is for; the leases end when it moves to done, dropped or open.
        #[arg(long)]
        task: Option<TaskId>,
        #[arg(long)]
        ttl_secs: Option<u32>,
    },
    /// Ask the user for a decision or an action.
    Ask {
        #[arg(help = at_most(
            "The question; it stands alone. Put background in the task body and name the task",
            limits().question_chars,
        ))]
        question: String,
        #[arg(long)]
        board: Option<BoardSlug>,
        #[arg(long)]
        task: Option<TaskId>,
        #[arg(long, default_value = "decision", value_parser = enum_arg::<AskKind>())]
        kind: AskKind,
        #[arg(long = "option", help = at_most(
            "A suggested answer, recommended first; repeat for more",
            ASK_OPTION_CHARS_MAX,
        ))]
        options: Vec<String>,
    },
    /// List asks.
    Asks {
        #[arg(long, default_value = "open")]
        status: String,
        #[arg(long)]
        board: Option<BoardSlug>,
    },
    /// Withdraw an open ask you made, such as a duplicate or one you no longer need.
    Withdraw { id: i64 },
    /// Answer an ask (user tokens only).
    Answer {
        id: i64,
        #[arg(help = at_most("The answer", limits().question_chars))]
        answer: String,
    },
    /// Join a board (default: the one for this repo).
    Join { board: Option<BoardSlug> },
    /// List boards.
    Boards,
    /// Create or change a board.
    #[command(subcommand)]
    Board(BoardCommand),
    /// Show the event log.
    Log {
        #[arg(long)]
        board: Option<BoardSlug>,
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Feedback-loop counts for the last N days: claims, takeovers, leases, asks, done.
    Stats {
        /// Days to count back from now, 1 to 365.
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=365))]
        days: u32,
    },
    /// API errors the server recorded, one line per signature, most first. Counts errors
    /// under the signal threshold, such as those since a fix deployed.
    Errors {
        /// From when: an RFC 3339 time, a date (YYYY-MM-DD) or a duration back from now
        /// (30m, 12h, 14d). Default: the server's signal window (signal_window_days).
        #[arg(long)]
        since: Option<String>,
        /// Only this error code, such as invalid.
        #[arg(long, value_parser = enum_arg::<board_proto::ErrorCode>())]
        code: Option<board_proto::ErrorCode>,
        /// Only this route, such as "POST /v1/tasks/{id}/notes".
        #[arg(long)]
        route: Option<String>,
        /// Only messages that contain this text, ignoring case.
        #[arg(long)]
        grep: Option<String>,
        /// Most signatures to list, 1 to 1000.
        #[arg(
            long,
            default_value_t = board_proto::ERRORS_LIMIT_DEFAULT,
            value_parser = clap::value_parser!(u32).range(1..=i64::from(board_proto::ERRORS_LIMIT_MAX))
        )]
        limit: u32,
    },
    /// Print your board snapshot.
    Snapshot {
        #[arg(long, default_value = "full", value_parser = enum_arg::<SnapshotKind>())]
        kind: SnapshotKind,
        #[arg(long, default_value = "lines", value_parser = enum_arg::<SnapshotFormat>())]
        format: SnapshotFormat,
        #[arg(long)]
        since: Option<i64>,
    },
    /// Report something in a setup that cost a step: the server files a feedback item, or
    /// adds the report to the live one with the same signature.
    Feedback {
        #[arg(long, default_value = "other", help = at_most(
            "The setup it is about, such as board, skills:rust or t3",
            FEEDBACK_ABOUT_MAX,
        ))]
        about: String,
        /// What you were doing.
        #[arg(long, value_parser = enum_arg::<FeedbackActivity>())]
        activity: Option<FeedbackActivity>,
        /// Default agent; signal is the server's own.
        #[arg(long, value_parser = enum_arg_hiding::<FeedbackSource>(&["signal"]))]
        source: Option<FeedbackSource>,
        #[arg(long, help = at_most(
            "Reports with equal signatures are one item (default: from about and the title)",
            FEEDBACK_SIGNATURE_MAX,
        ))]
        signature: Option<String>,
        #[arg(help = at_most("The first line becomes the title", limits().body_chars))]
        text: String,
    },
    /// Resolve a feedback item you own (or as the user); it moves to done.
    #[command(group(clap::ArgGroup::new("resolution").required(true)))]
    Resolve {
        id: TaskId,
        /// A task now does the work.
        #[arg(long, group = "resolution", value_name = "TASK")]
        routed: Option<TaskId>,
        /// The same as an earlier item, which takes later reports.
        #[arg(long, group = "resolution", value_name = "TASK")]
        duplicate: Option<TaskId>,
        #[arg(long, group = "resolution", value_name = "REASON", help = at_most(
            "Handed on outside the board: say where",
            RESOLUTION_REASON_CHARS_MAX,
        ))]
        forwarded: Option<String>,
        #[arg(long, group = "resolution", value_name = "REASON", help = at_most(
            "Not acted on: say why",
            RESOLUTION_REASON_CHARS_MAX,
        ))]
        declined: Option<String>,
        #[arg(long, group = "resolution", value_name = "WHAT CHANGED", help = at_most(
            "You fixed the cause yourself: say what changed",
            RESOLUTION_REASON_CHARS_MAX,
        ))]
        fixed: Option<String>,
    },
    /// Run the MCP server on stdio.
    Mcp,
    /// Handle a Claude Code or Codex hook event (reads its JSON from stdin).
    Hook {
        /// The event, such as post-tool-use or PostToolUse. An event this version does
        /// not know is ignored: exit code 2 would block the tool call on some events.
        event: String,
        #[arg(long, value_enum)]
        runtime: RuntimeArg,
    },
    /// Read a T3 thread's title, link and parent for a session's later hooks. The hooks
    /// start it when T3 answers too slowly for them.
    #[command(hide = true)]
    ReadThread {
        #[arg(long, value_enum)]
        runtime: RuntimeArg,
        session: String,
        thread: String,
    },
    /// Print or write runtime and service configuration.
    #[command(subcommand)]
    Install(crate::install::InstallCommand),
    /// Run the device daemon: coordinators and wake-ups.
    Watch {
        /// Run one reconcile pass and exit.
        #[arg(long)]
        once: bool,
    },
    /// Run the board server (on the server host).
    Serve {
        /// Server configuration (TOML; see docs/DEPLOY.md).
        #[arg(long, env = "BOARD_SERVER_CONFIG")]
        config: std::path::PathBuf,
    },
    /// Server administration on the server host, against the database file.
    #[command(subcommand)]
    Admin(AdminCommand),
}

/// Help for a text argument, with the server's limit.
fn at_most(what: &str, max: usize) -> String {
    format!("{what}. At most {max} characters")
}

/// Parses an enum flag the way the wire format spells it, listing its values in `--help`
/// and in the error for an unknown one.
fn enum_arg<T>() -> impl clap::builder::TypedValueParser<Value = T>
where
    T: serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    enum_arg_hiding(&[])
}

/// [`enum_arg`] that accepts but does not list the `hidden` values, which are kept only
/// for older callers.
fn enum_arg_hiding<T>(hidden: &[&str]) -> impl clap::builder::TypedValueParser<Value = T>
where
    T: serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    let values = ops::enum_values::<T>();
    // Holds for unit-variant enums with derived `Deserialize`; the CLI test builds every
    // command, so a flag of another kind of type fails there.
    debug_assert!(
        !values.is_empty() && values.iter().all(|v| parse_enum::<T>(v).is_ok()),
        "{} is not a unit-variant enum with derived Deserialize",
        std::any::type_name::<T>()
    );
    let values = values
        .iter()
        .map(|value| clap::builder::PossibleValue::new(*value).hide(hidden.contains(value)));
    clap::builder::PossibleValuesParser::new(values)
        .map(|value| parse_enum::<T>(&value).expect("a listed value parses"))
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RuntimeArg {
    Claude,
    Codex,
}

impl From<RuntimeArg> for board_proto::Runtime {
    fn from(value: RuntimeArg) -> Self {
        match value {
            RuntimeArg::Claude => Self::Claude,
            RuntimeArg::Codex => Self::Codex,
        }
    }
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Only tasks you own.
    #[arg(long)]
    mine: bool,
    /// Only ready tasks.
    #[arg(long)]
    ready: bool,
    /// Only pending proposals, which wait for another agent's verdict.
    #[arg(long)]
    proposals: bool,
    #[arg(long)]
    board: Option<BoardSlug>,
    #[arg(long, value_parser = enum_arg::<TaskStatus>())]
    status: Option<TaskStatus>,
    #[arg(long)]
    owner: Option<board_proto::AgentId>,
    /// Only the direct children of this task.
    #[arg(long)]
    parent: Option<TaskId>,
    #[arg(long)]
    limit: Option<u32>,
    /// Include bodies and acceptance checks.
    #[arg(long)]
    detailed: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    #[arg(help = at_most(
        "Imperative title, no status or ids; put detail in --body",
        limits().title_chars,
    ))]
    title: String,
    #[arg(long)]
    board: Option<BoardSlug>,
    #[arg(long, default_value = "", help = at_most("Markdown that stands alone", limits().body_chars))]
    body: String,
    #[arg(long = "accept", default_value = "", help = at_most(
        "How done is checked: one observable check per line",
        limits().body_chars,
    ))]
    acceptance: String,
    #[arg(long)]
    parent: Option<TaskId>,
    /// A task that must be done first, on any board; repeat for more.
    #[arg(long = "blocker")]
    blockers: Vec<TaskId>,
    #[arg(long, value_parser = enum_arg::<Priority>())]
    priority: Option<Priority>,
    /// May the coordinator start it without the user? Default: the board's autopilot. Set
    /// false only when the user must act or decide; waiting on a task is a blocker. A
    /// proposal (a body starting "Proposal:") is always stored not auto until another agent
    /// accepts it.
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    auto: Option<bool>,
    /// Not ready before this date (YYYY-MM-DD, UTC) or RFC 3339 time.
    #[arg(long)]
    not_before: Option<String>,
    /// Create it in doing, owned by you, so no other agent starts it first.
    #[arg(long)]
    claim: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    id: TaskId,
    /// Done goes through `board done`, with evidence.
    #[arg(long, value_parser = enum_arg_hiding::<TaskStatus>(&["done"]))]
    status: Option<TaskStatus>,
    #[arg(long, help = at_most("New title", limits().title_chars))]
    title: Option<String>,
    #[arg(long, help = at_most("New body", limits().body_chars))]
    body: Option<String>,
    #[arg(long = "accept", help = at_most("New acceptance checks", limits().body_chars))]
    acceptance: Option<String>,
    #[arg(long, value_parser = enum_arg::<Priority>())]
    priority: Option<Priority>,
    /// Comma-separated blockers; an empty value clears them.
    #[arg(long)]
    blockers: Option<String>,
    #[arg(long = "reason", help = at_most("Why it is blocked", limits().note_chars))]
    blocked_reason: Option<String>,
    /// May the coordinator start it? Set false only when the user must act or decide, or for
    /// a pending proposal; set a proposal true only on an accept verdict.
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    auto: Option<bool>,
    /// Not ready before this date (YYYY-MM-DD, UTC) or RFC 3339 time; none clears it.
    #[arg(long)]
    not_before: Option<String>,
    /// Hand the task to this agent id (owner, the board's coordinator or the user only).
    #[arg(long)]
    owner: Option<board_proto::AgentId>,
}

#[derive(Debug, Args)]
pub struct DoneArgs {
    id: TaskId,
    #[arg(long = "check", help = at_most(
        "A check command that was run; pair each with --exit",
        EVIDENCE_VALUE_CHARS_MAX,
    ))]
    checks: Vec<String>,
    /// Exit code of the matching --check.
    #[arg(long = "exit", allow_negative_numbers = true)]
    exits: Vec<i32>,
    #[arg(long = "commit", help = at_most("A commit SHA", EVIDENCE_VALUE_CHARS_MAX))]
    commits: Vec<String>,
    #[arg(long = "pr", help = at_most("A pull request URL", EVIDENCE_VALUE_CHARS_MAX))]
    prs: Vec<String>,
    #[arg(long = "artifact", help = at_most(
        "An artifact link or path, such as a log",
        EVIDENCE_VALUE_CHARS_MAX,
    ))]
    artifacts: Vec<String>,
    #[arg(long, help = at_most(
        "For the user: what changed and what they should know",
        SUMMARY_CHARS_MAX,
    ))]
    summary: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum BoardCommand {
    /// Create a board.
    Create {
        slug: BoardSlug,
        #[arg(long, help = at_most("The board's title", limits().title_chars))]
        title: String,
        #[arg(long, default_value = "", help = at_most("What the board is for", limits().body_chars))]
        goal: String,
        /// A repo key such as github.com/owner/repo; repeat for more.
        #[arg(long = "repo")]
        repos: Vec<String>,
        #[arg(long)]
        autopilot: bool,
        /// A device allowed to host the coordinator, in order of preference.
        #[arg(long = "home-device")]
        home_devices: Vec<DeviceName>,
        /// For a test or a one-off: the server closes the board once idle (a day by
        /// default) and deletes it later (a week after closing by default).
        #[arg(long)]
        temporary: bool,
    },
    /// Change a board.
    Update {
        slug: BoardSlug,
        #[arg(long, help = at_most("New title", limits().title_chars))]
        title: Option<String>,
        #[arg(long, help = at_most("New goal", limits().body_chars))]
        goal: Option<String>,
        #[arg(long, value_parser = enum_arg::<BoardStatus>())]
        status: Option<BoardStatus>,
        /// Replace the repos; repeat for more.
        #[arg(long = "repo")]
        repos: Option<Vec<String>>,
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        autopilot: Option<bool>,
        /// Replace the home devices; repeat for more.
        #[arg(long = "home-device")]
        home_devices: Option<Vec<DeviceName>>,
        /// Whether the server closes the board once idle and deletes it later. Only the
        /// user may turn it on for an existing board.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        temporary: Option<bool>,
        /// How many tasks the coordinator and its child threads may have in progress at
        /// once (1 to 4). Only the user may raise it.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(board_proto::PARALLEL_TASKS_MAX)))]
        parallel_tasks: Option<u32>,
    },
    /// Delete a closed board without open asks, with its tasks, notes, asks and history.
    /// User tokens only; it cannot be undone.
    Delete { slug: BoardSlug },
    /// Make an existing T3 thread on this device the board's coordinator, then switch
    /// autopilot on. Run it on a home device whose daemon (`board watch`) then briefs the
    /// thread instead of starting a new one.
    Adopt {
        slug: BoardSlug,
        /// The T3 thread id, as in its link or `t3 thread show`.
        thread: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Create a device and print its token once. List and revoke devices through
    /// `/v1/admin/devices` as the user.
    Device {
        name: DeviceName,
        /// The token acts as the user (answers asks, administers devices).
        #[arg(long)]
        user: bool,
        /// The server's database file.
        #[arg(long, env = "BOARD_DB")]
        db: std::path::PathBuf,
    },
    /// Create a one-time code (valid 15 minutes) that pairs a Telegram chat: send the bot
    /// `/start <code>` in a private chat. Not needed when server.toml sets `[telegram]
    /// chat_id`, which always wins.
    TelegramPair {
        /// Allow the code to replace the paired chat; it stays paired until the code is used.
        #[arg(long)]
        replace: bool,
        /// The server's database file.
        #[arg(long, env = "BOARD_DB")]
        db: std::path::PathBuf,
    },
    /// Write a consistent copy of the database (`VACUUM INTO`) to a new file.
    Backup {
        /// The server's database file.
        #[arg(long, env = "BOARD_DB")]
        db: std::path::PathBuf,
        /// The copy to write; must not exist.
        #[arg(long)]
        out: std::path::PathBuf,
    },
}

fn report(result: anyhow::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("board: {err:#}");
            1
        }
    }
}

async fn serve(path: &std::path::Path) -> anyhow::Result<()> {
    let mut config = board_server::ServerConfig::load(path)?;
    config.commit = crate::COMMIT.to_owned();
    config.built = crate::BUILT.to_owned();
    board_server::serve(config).await?;
    Ok(())
}

fn admin(cmd: AdminCommand) -> anyhow::Result<()> {
    match cmd {
        AdminCommand::Device { name, user, db } => {
            let token = board_server::create_device(&db, &name, user)?;
            println!("{token}");
            eprintln!(
                "board: device {name} created; this token is shown once. On the device run \
                 `board login --server <url> --device {name}` and paste it."
            );
        }
        AdminCommand::TelegramPair { replace, db } => {
            let code = board_server::telegram_pair(&db, replace)?;
            println!("{code}");
            eprintln!(
                "board: pairing code created; it works once, within 15 minutes. In Telegram, \
                 open a private chat with your bot and send: /start {code}"
            );
        }
        AdminCommand::Backup { db, out } => {
            board_server::backup(&db, &out)?;
            eprintln!("board: wrote {}", out.display());
        }
    }
    Ok(())
}

impl Command {
    /// Refuses text over the server's limits before any request is sent.
    fn check_text_limits(&self) -> Result<()> {
        let l = limits();
        let check_board_title = |title: &str| {
            ops::check_chars_or(
                "title",
                title,
                l.title_chars,
                Some(board_proto::BOARD_TITLE_LONGER_TEXT_GOES),
            )
        };
        let check_all = |field, values: &[String], max| {
            values
                .iter()
                .try_for_each(|value| check_chars(field, value, max))
        };
        let check_opt = |field, value: &Option<String>, max| {
            value
                .as_deref()
                .map_or(Ok(()), |value| check_chars(field, value, max))
        };
        match self {
            Self::Add(a) => {
                check_chars("title", &a.title, l.title_chars)?;
                check_chars("body", &a.body, l.body_chars)?;
                check_chars("acceptance", &a.acceptance, l.body_chars)?;
            }
            Self::Update(a) => {
                check_opt("title", &a.title, l.title_chars)?;
                check_opt("body", &a.body, l.body_chars)?;
                check_opt("acceptance", &a.acceptance, l.body_chars)?;
                check_opt("blocked_reason", &a.blocked_reason, l.note_chars)?;
            }
            Self::Done(a) => {
                for values in [&a.checks, &a.commits, &a.prs, &a.artifacts] {
                    check_all("evidence value", values, EVIDENCE_VALUE_CHARS_MAX)?;
                }
                check_opt("summary", &a.summary, SUMMARY_CHARS_MAX)?;
            }
            Self::Note { text, .. } => check_chars("text", text, l.note_chars)?,
            Self::Handoff {
                now,
                next,
                blockers,
                ..
            } => {
                check_opt("now", now, l.handoff_now_max())?;
                check_opt("next", next, l.handoff_field_chars)?;
                check_chars("blockers", blockers, l.handoff_field_chars)?;
            }
            Self::Ask {
                question, options, ..
            } => {
                check_chars("question", question, l.question_chars)?;
                check_all("option", options, ASK_OPTION_CHARS_MAX)?;
            }
            Self::Answer { answer, .. } => check_chars("answer", answer, l.question_chars)?,
            Self::Errors { grep, .. } => check_opt("q", grep, board_proto::ERRORS_Q_CHARS_MAX)?,
            Self::Feedback {
                about,
                signature,
                text,
                ..
            } => {
                // A plain error, not reported: the server answers an over-long about
                // with `about "…" is not a setup name`, under another signature.
                check_chars("about", about, FEEDBACK_ABOUT_MAX)
                    .map_err(|err| anyhow::anyhow!("{err}"))?;
                check_opt("signature", signature, FEEDBACK_SIGNATURE_MAX)?;
                ops::check_chars_or(
                    "text",
                    text.trim(),
                    l.body_chars,
                    Some(board_proto::FEEDBACK_LONGER_TEXT_GOES),
                )?;
            }
            Self::Resolve {
                forwarded,
                declined,
                fixed,
                ..
            } => {
                check_opt("reason", forwarded, RESOLUTION_REASON_CHARS_MAX)?;
                check_opt("reason", declined, RESOLUTION_REASON_CHARS_MAX)?;
                check_opt("reason", fixed, RESOLUTION_REASON_CHARS_MAX)?;
            }
            Self::Board(BoardCommand::Create { title, goal, .. }) => {
                check_board_title(title)?;
                check_chars("goal", goal, l.body_chars)?;
            }
            Self::Board(BoardCommand::Update { title, goal, .. }) => {
                if let Some(title) = title {
                    check_board_title(title)?;
                }
                check_opt("goal", goal, l.body_chars)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The route template of the request whose text [`Self::check_text_limits`] checks, as
    /// the server records its errors.
    fn checked_route(&self) -> Option<&'static str> {
        Some(match self {
            Self::Add(_) => "POST /v1/tasks",
            Self::Update(_) => "PATCH /v1/tasks/{id}",
            Self::Done(_) => "POST /v1/tasks/{id}/done",
            Self::Note { .. } => "POST /v1/tasks/{id}/notes",
            // Without both, it shows the handoff and sends no text.
            Self::Handoff {
                now: Some(_),
                next: Some(_),
                ..
            } => "PUT /v1/agents/{id}/handoff",
            Self::Ask { .. } => "POST /v1/asks",
            Self::Answer { .. } => "POST /v1/asks/{id}/answer",
            Self::Errors { .. } => "GET /v1/errors",
            Self::Feedback { .. } => "POST /v1/feedback",
            Self::Resolve { .. } => "POST /v1/feedback/{id}/resolve",
            Self::Board(BoardCommand::Create { .. }) => "POST /v1/boards",
            Self::Board(BoardCommand::Update { .. }) => "PATCH /v1/boards/{slug}",
            _ => return None,
        })
    }

    /// Commands that act as the calling agent, which must then be registered.
    fn acts_as_agent(&self) -> bool {
        matches!(
            self,
            Self::Status { .. }
                | Self::Add(_)
                | Self::Claim { .. }
                | Self::Release { .. }
                | Self::Take { .. }
                | Self::Update(_)
                | Self::Done(_)
                | Self::Note { .. }
                | Self::Handoff { .. }
                | Self::Lease { .. }
                | Self::Ask { .. }
                | Self::Join { .. }
                | Self::Snapshot { .. }
                | Self::Feedback { .. }
                | Self::Resolve { .. }
        )
    }
}

/// Runs the CLI and returns the process exit code.
pub async fn run(cli: Cli) -> i32 {
    match cli.command {
        // Hooks never fail the turn.
        Command::Hook { event, runtime } => {
            match <crate::hook::HookEvent as ValueEnum>::from_str(&event, false) {
                Ok(event) => crate::hook::run(event, runtime.into(), cli.agent.as_deref()).await,
                Err(_) => tracing::debug!("hook {event}: unknown event, ignored"),
            }
            return 0;
        }
        Command::ReadThread {
            runtime,
            session,
            thread,
        } => {
            crate::hook::read_thread(runtime.into(), &session, &thread).await;
            return 0;
        }
        Command::Serve { config } => {
            return report(serve(&config).await);
        }
        Command::Admin(cmd) => return report(admin(cmd)),
        _ => {}
    }
    let code = match dispatch(cli).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("board: {err:#}");
            1
        }
    };
    if let Some(warning) = crate::client::take_skew_warning(crate::client::Reporter::Cli) {
        eprintln!("board: warning: {warning}");
    }
    code
}

/// How long a refused command waits for the server to record the refusal.
const REFUSAL_REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Records a refusal made before sending, so the server's signals count it like its own
/// (`POST /v1/errors`). Best effort: a failure is logged at debug level only, so the
/// command's output and exit code stay as they were.
async fn report_refusal(ctx: &Ctx, route: &str, refused: &ops::Refused) {
    if ctx.client.agent().is_none() {
        return;
    }
    let body = board_proto::ClientError {
        code: board_proto::ErrorCode::Invalid,
        route: route.to_owned(),
        message: refused.message.clone(),
    };
    if let Err(err) = ctx
        .client
        .report_refusal(&body, REFUSAL_REPORT_TIMEOUT)
        .await
    {
        tracing::debug!("recording the refusal on the server failed: {err:#}");
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    let home = Home::from_env()?;
    let json = cli.json;
    let agent = || agent_from_env(cli.agent.as_deref());
    let loaded = std::cell::OnceCell::new();
    let ctx = || -> Result<Ctx> {
        if let Some(ctx) = loaded.get() {
            return Ok(Ctx::clone(ctx));
        }
        if let Some(named) = &cli.agent {
            let named: board_proto::AgentId = named.parse().context("--agent")?;
            crate::identity::refuse_borrowed(proven_identity().as_ref(), &named, "--agent")?;
        }
        let mut ctx = Ctx::load(home.clone(), agent()?, None)?;
        // A read the proxy fails while the server restarts is sent again, not left to the agent.
        ctx.client = ctx
            .client
            .with_read_retries(crate::client::READ_RETRY_DELAYS);
        Ok(loaded.get_or_init(|| ctx).clone())
    };
    if let Err(err) = cli.command.check_text_limits() {
        if let (Some(refused), Some(route)) = (
            err.downcast_ref::<ops::Refused>(),
            cli.command.checked_route(),
        ) {
            match ctx() {
                Ok(ctx) => report_refusal(&ctx, route, refused).await,
                Err(err) => tracing::debug!("not recording the refusal on the server: {err:#}"),
            }
        }
        return Err(err);
    }
    if cli.command.acts_as_agent() {
        let explicit = cli.agent.as_ref().map(|_| "--agent");
        ctx()?.ensure_registered(explicit, cli.register).await?;
    }
    let out: Output = match cli.command {
        Command::Login { server, device } => return login(&home, server, device),
        Command::Install(cmd) => return crate::install::run(cmd),
        Command::Mcp => return crate::mcp::serve(home, cli.agent, cli.register).await,
        Command::Watch { once } => return crate::daemon::run(home, once).await,
        Command::Status { board, detailed } => ops::status(&ctx()?, board, detailed).await?,
        Command::Ls(a) => {
            let query = TaskQuery {
                board: a.board,
                status: a.status,
                owner: a.owner,
                ready: a.ready.then_some(true),
                proposal: a.proposals.then_some(true),
                mine: a.mine.then_some(true),
                parent: a.parent,
                limit: a.limit,
            };
            let ctx = ctx()?;
            if a.mine {
                ctx.agent()?;
            }
            ops::task_list(&ctx, &query, a.detailed).await?
        }
        Command::Show { id, detailed } => ops::task_show(&ctx()?, &id, detailed).await?,
        Command::Add(a) => {
            let new = ops::NewTask {
                board: a.board,
                title: a.title,
                body: a.body,
                acceptance: a.acceptance,
                parent: a.parent,
                blockers: a.blockers,
                priority: a.priority,
                auto: a.auto,
                not_before: ops::parse_not_before(a.not_before.as_deref())?.flatten(),
                claim: a.claim,
            };
            ops::task_add(&ctx()?, new).await?
        }
        Command::Claim { id } => ops::task_action(&ctx()?, &id, "claim").await?,
        Command::Release { id } => ops::task_action(&ctx()?, &id, "release").await?,
        Command::Take { id } => ops::task_action(&ctx()?, &id, "take").await?,
        Command::Update(a) => {
            let blockers = a.blockers.map(|list| parse_task_list(&list)).transpose()?;
            let changes = ops::TaskChanges {
                status: a.status,
                title: a.title,
                body: a.body,
                acceptance: a.acceptance,
                priority: a.priority,
                blockers,
                blocked_reason: a.blocked_reason,
                auto: a.auto,
                not_before: ops::parse_not_before(a.not_before.as_deref())?,
                owner: a.owner,
            };
            ops::task_update(&ctx()?, &a.id, changes).await?
        }
        Command::Done(a) => {
            if a.checks.len() != a.exits.len() {
                bail!("pair every --check with an --exit code");
            }
            let evidence = ops::Evidence {
                checks: a.checks.into_iter().zip(a.exits).collect(),
                commits: a.commits,
                prs: a.prs,
                artifacts: a.artifacts,
            };
            ops::task_done(&ctx()?, &a.id, evidence, a.summary).await?
        }
        Command::Note { id, text, kind } => ops::task_note(&ctx()?, &id, kind, text).await?,
        Command::Handoff {
            now,
            next,
            blockers,
            of,
        } => match (now, next) {
            (Some(now), Some(next)) => ops::handoff_set(&ctx()?, now, next, blockers).await?,
            _ => ops::handoff_show(&ctx()?, of).await?,
        },
        Command::Lease {
            globs,
            task,
            ttl_secs,
        } => ops::lease_paths(&ctx()?, globs, task, ttl_secs).await?,
        Command::Ask {
            question,
            board,
            task,
            kind,
            options,
        } => {
            let ask = ops::NewAsk {
                board,
                task,
                kind,
                question,
                options,
            };
            ops::ask_user(&ctx()?, ask).await?
        }
        Command::Asks { status, board } => {
            let status = (status != "all").then_some(status.as_str());
            ops::asks(&ctx()?, status, board.as_ref()).await?
        }
        Command::Withdraw { id } => ops::ask_withdraw(&ctx()?, id).await?,
        Command::Answer { id, answer } => ops::answer(&ctx()?, id, &answer).await?,
        Command::Join { board } => ops::join(&ctx()?, board).await?,
        Command::Boards => ops::boards(&ctx()?).await?,
        Command::Board(BoardCommand::Create {
            slug,
            title,
            goal,
            repos,
            autopilot,
            home_devices,
            temporary,
        }) => {
            let body = CreateBoard {
                slug,
                title,
                goal,
                repos,
                autopilot,
                home_devices,
                temporary,
            };
            ops::board_create(&ctx()?, body).await?
        }
        Command::Board(BoardCommand::Update {
            slug,
            title,
            goal,
            status,
            repos,
            autopilot,
            home_devices,
            temporary,
            parallel_tasks,
        }) => {
            let body = UpdateBoard {
                title,
                goal,
                status,
                repos,
                autopilot,
                home_devices,
                temporary,
                parallel_tasks,
            };
            ops::board_update(&ctx()?, &slug, body).await?
        }
        Command::Board(BoardCommand::Delete { slug }) => ops::board_delete(&ctx()?, &slug).await?,
        Command::Board(BoardCommand::Adopt { slug, thread }) => {
            let ctx = ctx()?;
            let t3 = crate::t3::T3Cli::locate(ctx.config.t3_bin.as_deref());
            ops::board_adopt(&ctx, &t3, &slug, &thread, chrono::Utc::now()).await?
        }
        Command::Log {
            board,
            since,
            limit,
        } => ops::log(&ctx()?, since, board.as_ref(), Some(limit)).await?,
        Command::Snapshot {
            kind,
            format,
            since,
        } => ops::snapshot(&ctx()?, kind, format, since).await?,
        Command::Feedback {
            about,
            activity,
            source,
            signature,
            text,
        } => {
            let req = CreateFeedback {
                about,
                text,
                activity,
                source,
                signature,
            };
            ops::feedback(&ctx()?, req).await?
        }
        Command::Resolve {
            id,
            routed,
            duplicate,
            forwarded,
            declined,
            fixed,
        } => {
            let (kind, task, reason) = match (routed, duplicate, forwarded, declined, fixed) {
                (Some(task), ..) => (ResolutionKind::Routed, Some(task), None),
                (_, Some(task), ..) => (ResolutionKind::Duplicate, Some(task), None),
                (_, _, Some(reason), ..) => (ResolutionKind::Forwarded, None, Some(reason)),
                (_, _, _, Some(reason), _) => (ResolutionKind::Declined, None, Some(reason)),
                (.., Some(reason)) => (ResolutionKind::Fixed, None, Some(reason)),
                _ => unreachable!("clap requires one resolution"),
            };
            let req = ResolveFeedback { kind, task, reason };
            ops::resolve(&ctx()?, &id, req).await?
        }
        Command::Stats { days } => ops::stats(&ctx()?, days, chrono::Utc::now()).await?,
        Command::Errors {
            since,
            code,
            route,
            grep,
            limit,
        } => {
            let since = since
                .map(|s| ops::parse_since(&s, chrono::Utc::now()))
                .transpose()?;
            let q = crate::client::ErrorsQuery {
                since,
                code,
                route,
                q: grep,
                limit,
            };
            ops::errors(&ctx()?, &q).await?
        }
        Command::Hook { .. }
        | Command::ReadThread { .. }
        | Command::Serve { .. }
        | Command::Admin(_) => {
            unreachable!("handled in run")
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&out.json)?);
    } else {
        println!("{}", out.text);
    }
    Ok(())
}

fn parse_task_list(list: &str) -> Result<Vec<TaskId>> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().with_context(|| format!("blocker {s:?}")))
        .collect()
}

fn login(home: &Home, server: String, device: DeviceName) -> Result<()> {
    let server = server.trim().trim_end_matches('/').to_owned();
    if !(server.starts_with("http://") || server.starts_with("https://")) {
        bail!("--server must be an http(s) URL");
    }
    let mut token = String::new();
    std::io::stdin()
        .read_to_string(&mut token)
        .context("reading the token from stdin")?;
    let token = token.trim();
    if token.is_empty() {
        bail!("pipe the device token on stdin, for example: board login ... < token-file");
    }
    let mut config = Config::load(home)?;
    config.server = Some(server.clone());
    config.device = Some(device.clone());
    config.save(home)?;
    let stored = store_token(home, token)?;
    let place = match stored.store {
        TokenStore::Keychain => "the macOS Keychain".to_owned(),
        TokenStore::File => format!("{} (mode 0600)", home.token_path().display()),
    };
    if let Some(note) = stored.note {
        eprintln!("board: {note}");
    }
    println!("logged in to {server} as device {device}; token stored in {place}");
    Ok(())
}

/// Path argument helper for install commands.
pub fn expand_tilde(path: PathBuf) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map_or(path.clone(), |h| h.join(rest)),
        Err(_) => path,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn every_command_builds_with_its_enum_values() {
        // Builds every value parser, which checks each enum flag's values parse.
        Cli::command().debug_assert();
        let update = Cli::command()
            .find_subcommand_mut("update")
            .unwrap()
            .render_help()
            .to_string();
        assert!(
            update.contains("[possible values: open, doing, blocked, review, dropped]"),
            "done goes through board done: {update}"
        );
    }

    #[test]
    fn every_command_names_its_json_shape_in_help() {
        fn walk(cmd: &clap::Command, path: &str, seen: &mut Vec<String>) {
            for sub in cmd.get_subcommands() {
                let name = sub.get_name();
                let path = if path.is_empty() {
                    name.to_owned()
                } else {
                    format!("{path} {name}")
                };
                // clap's own `help` subcommand, and its mirror of the tree, take no flags.
                if name == "help" {
                    continue;
                }
                let json = sub
                    .get_arguments()
                    .find(|a| a.get_id() == "json")
                    .unwrap_or_else(|| panic!("board {path} has no --json"));
                let help = json.get_help().map(ToString::to_string).unwrap_or_default();
                assert!(
                    help.starts_with("Print JSON instead of text: ")
                        || help.starts_with("Ignored: "),
                    "board {path} --help does not name its --json shape: {help:?}; \
                     add it to JSON_SHAPES or JSON_IGNORED"
                );
                seen.push(path.clone());
                walk(sub, &path, seen);
            }
        }
        let cmd = command();
        let mut seen = Vec::new();
        walk(&cmd, "", &mut seen);
        for (path, _) in JSON_SHAPES {
            assert!(seen.iter().any(|s| s == path), "no command {path:?}");
        }
        for path in JSON_IGNORED {
            assert!(seen.iter().any(|s| s == path), "no command {path:?}");
        }

        let mut cmd = command();
        for (args, shape) in [
            (&["asks"][..], "a JSON array of asks"),
            (&["log"], "{events: [...], next_since}"),
            (&["board", "delete"], "{deleted: slug}"),
        ] {
            let err = cmd
                .try_get_matches_from_mut([&["board"][..], args, &["--help"]].concat())
                .unwrap_err();
            let help = err.to_string();
            assert!(help.contains(shape), "board {args:?} --help: {help}");
        }
    }

    #[test]
    fn every_refusal_names_a_route_the_server_records() {
        let long = "x".repeat(5000);
        let long = long.as_str();
        let cases: &[&[&str]] = &[
            &["add", long],
            &["update", "factory-1", "--title", long],
            &["done", "factory-1", "--pr", long],
            &["done", "factory-1", "--pr", "x", "--summary", long],
            &["note", "factory-1", long],
            &["handoff", "--now", long, "--next", "x"],
            &["ask", long],
            &["answer", "7", long],
            &["errors", "--grep", long],
            &["feedback", long],
            &["resolve", "board-feedback-1", "--declined", long],
            &["board", "create", "factory", "--title", long],
            &["board", "update", "factory", "--goal", long],
        ];
        let parse = |args: &[&str]| Cli::try_parse_from([&["board"][..], args].concat()).unwrap();
        for args in cases {
            let cli = parse(args);
            let err = cli.command.check_text_limits().unwrap_err();
            let refused = err.downcast_ref::<ops::Refused>().unwrap();
            assert!(
                board_proto::TooLong::parse(&refused.message).is_some(),
                "{args:?}: the server would refuse {refused}"
            );
            let route = cli.command.checked_route().unwrap();
            assert!(
                board_proto::CLIENT_ERROR_ROUTES.contains(&route),
                "{args:?}: {route}"
            );
        }

        // Not reported: a handoff that only shows, and an about the server refuses with
        // another message.
        let show = parse(&["handoff", "--blockers", long]);
        assert!(show.command.check_text_limits().is_err());
        assert_eq!(show.command.checked_route(), None);
        let about = parse(&["feedback", "--about", long, "x"]);
        let err = about.command.check_text_limits().unwrap_err();
        assert!(err.downcast_ref::<ops::Refused>().is_none(), "{err}");
    }
}
