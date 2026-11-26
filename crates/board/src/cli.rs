//! Command-line interface.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use board_proto::{
    ASK_OPTION_CHARS_MAX, AdminUpdateDevice, AskKind, BoardSlug, BoardStatus, CreateBoard,
    CreateFeedback, DeviceName, DeviceRole, EVIDENCE_VALUE_CHARS_MAX, FEEDBACK_ABOUT_MAX,
    FEEDBACK_SIGNATURE_MAX, FeedbackActivity, FeedbackSource, NoteKind, Priority,
    RESOLUTION_REASON_CHARS_MAX, RUN_CANCEL_REASON_CHARS_MAX, RUN_EVENTS_LIMIT_MAX,
    RUN_STEER_MESSAGE_CHARS_MAX, RUNS_LIMIT_MAX, ResolutionKind, ResolveFeedback, RunFilter, RunId,
    RunKind, RunOrigin, RunState, SUMMARY_CHARS_MAX, SnapshotFormat, SnapshotKind, Taint,
    TaintEffect, TaskId, TaskStatus, UpdateBoard,
};
use clap::builder::TypedValueParser as _;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::client::TaskQuery;
use crate::config::{Config, Home, TokenStore, store_token};
use crate::identity::{agent_from_env, proven_identity};
use crate::ops::{self, Ctx, Output, TITLE_AIM_CHARS, check_chars, limits, parse_enum};

#[derive(Debug, Parser)]
#[command(
    name = "board",
    version = crate::BUILD,
    about = "Shared work board for coding-agent threads"
)]
pub struct Cli {
    /// Act as this agent id instead of the one from the environment. In a T3 thread, only
    /// the thread's own id is accepted. A command that acts as the agent refuses an id the
    /// server does not know, unless --register is given too.
    #[arg(long, global = true)]
    pub agent: Option<String>,
    /// Register the --agent id when the server does not know it yet, before a command that
    /// acts as it (also `board mcp`): for a script that acts under its own fixed id.
    /// Sessions register through their hooks instead.
    #[arg(long, global = true)]
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
    (
        "rank",
        "{tasks: [...]}, the ranked tasks in the order named",
    ),
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
    ("pause", "a JSON array of the boards it paused"),
    ("resume", "a JSON array of the boards it resumed"),
    (
        "usage",
        "the reading {five_hour?, seven_day?, read_at, device}, or null when none",
    ),
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
    ("device", "see each subcommand's --help"),
    (
        "device wait",
        "{device?, running, limit, slots, free, waited_secs}",
    ),
    ("devices", "a JSON array of devices"),
    ("runs", "a JSON array of runs"),
    (
        "logs",
        "a JSON array of run events (or one event object per line with --follow)",
    ),
    ("steer", "{wake_id, run_id, device}"),
    ("cancel", "{run_id, device, state, wake_id?}"),
    (
        "attach",
        "{run_id, device, local, runner, state, workdir?, branch?, t3_link?, command, commands}",
    ),
    ("admin", "see each subcommand's --help"),
    (
        "admin device-update",
        "the device object {name, role, status, ...}",
    ),
    (
        "admin replay-schedule",
        "{since, until, days, historical_tasks, historical_runs, baseline, candidate?}",
    ),
    ("approved", "see each subcommand's --help"),
    (
        "approved board",
        "the board object {slug, title, status, ...}",
    ),
    (
        "approved board update",
        "the board object {slug, title, status, ...}",
    ),
    ("approved usage", "{reserve, until?, previous?}"),
    ("log", "{events: [...], next_since}"),
    (
        "stats",
        "{days, tasks_created, tasks_done, ..., boards: [...]} (or SchedulerStats with --scheduler)",
    ),
    ("errors", "{signatures: [...], since, retention_days}"),
    ("snapshot", "{kind, text, seq, at}"),
    ("feedback", "{task, duplicate, reports}"),
    ("resolve", "the task object {id, status, owner, ...}"),
    (
        "report-host",
        "{host, collector, health_pushed, schedule_sources, total_jobs} (or {health, schedules} with --dry-run)",
    ),
];

/// Commands that print no JSON and ignore `--json`, with their subcommands.
const JSON_IGNORED: &[&str] = &[
    "login",
    "mcp",
    "mcp-http",
    "hook",
    "read-thread",
    "install",
    "brief",
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
        /// Also the goal, and every ended agent of the last 7 days instead of the ones seen last.
        #[arg(long)]
        detailed: bool,
    },
    /// List tasks.
    Ls(LsArgs),
    /// Show a task with its evidence, every finding and decision note, and the latest plain
    /// notes (--detailed for all and the history).
    /// With several ids, shows each task in a compact form in one call: state, evidence,
    /// text, findings and decisions (at most 20; --detailed is for one task).
    /// The text of one task ends with a number: pass it as --since when you read the task
    /// again, to see only what changed.
    Show {
        #[arg(required = true)]
        ids: Vec<TaskId>,
        #[arg(long)]
        detailed: bool,
        /// Only what changed on one task after this: the number the last show ended with,
        /// a time, or a duration back such as 2h.
        #[arg(long)]
        since: Option<String>,
    },
    /// Add a task.
    Add(AddArgs),
    /// Claim a task: it moves to doing with you as owner. Also resumes your own, or
    /// nobody's, task in review or blocked, unless an open ask, or a blocked reason
    /// someone else set, holds it. Pending proposals (the snapshot's "proposals to review")
    /// are not work and cannot be claimed: a reviewer (not the proposer) writes a `verdict:`
    /// decision note and sets `--auto true` (`board update <id> --auto true`) first.
    Claim { id: TaskId },
    /// Release a task you own back to open. First add a note that says which steps are
    /// done, and file what is not in the task (follow-up tasks, proposals).
    Release { id: TaskId },
    /// Take a task whose owner is stale, or whose owner's session ended when you created
    /// the task or delegated that owner.
    Take { id: TaskId },
    /// Change a task's fields or status.
    Update(UpdateArgs),
    /// Order tasks inside their priority levels: the tasks named start in this order, each
    /// before the unranked tasks of its level. One board per call, at most 500 tasks.
    Rank {
        /// Task ids, first to start first.
        #[arg(required = true)]
        ids: Vec<TaskId>,
        /// Clear the rank of every task of the board that is not named.
        #[arg(long)]
        clear_others: bool,
    },
    /// Mark a task done with evidence, only when every acceptance check passes; file what
    /// remains first (sub-tasks, proposals).
    Done(DoneArgs),
    /// Add a note to a task.
    Note {
        id: TaskId,
        #[arg(help = at_most(
            "The note: cite evidence. Split longer text across notes, \
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
        /// Directory in the git checkout to lease in; defaults to the current checkout.
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
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
        /// Attach a change to the board: the server puts the option that applies it first
        /// and applies it when the user picks that option, so no step of yours follows.
        /// For the settings only the user may raise: how many tasks run at once (1 to 4).
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(board_proto::PARALLEL_TASKS_MAX)))]
        set_parallel_tasks: Option<u32>,
        /// Attach a change to the board's usage stop percent (1 to 100), applied as above.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        set_usage_stop_at: Option<u32>,
        /// Attach a change to whether the board is temporary, applied as above.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        set_temporary: Option<bool>,
    },
    /// List asks, the newest when more match than the limit.
    Asks {
        /// open, answered, withdrawn or all.
        #[arg(long, default_value = "open")]
        status: String,
        #[arg(long)]
        board: Option<BoardSlug>,
        /// The most asks to list, the newest. Default: every open ask, or 50 of another
        /// status; --json then holds the same cut list.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..).map(|n| n as usize))]
        limit: Option<usize>,
    },
    /// Withdraw an open ask you made, such as a duplicate or one you no longer need.
    Withdraw { id: i64 },
    /// Answer an ask (user tokens only).
    Answer {
        id: i64,
        #[arg(help = at_most("The answer", limits().question_chars))]
        answer: String,
    },
    /// Apply a change the user approved in their own words, with the user's login on this
    /// device. Use it only after the user approved this exact change; otherwise ask them
    /// (`board ask --set-...`). The event log records who ran it and the words quoted.
    #[command(subcommand)]
    Approved(ApprovedCommand),
    /// Join a board (default: the one for this repo).
    Join { board: Option<BoardSlug> },
    /// List boards.
    Boards {
        /// Only the boards that cover this repository: its URL, or its key such as
        /// github.com/owner/name.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Pause all your work: every active board, or only those named. The workers
    /// and judges the daemon started and the agents on their tasks stop at their next hook, and
    /// the daemon starts nothing on them. `board resume` undoes it. The feedback board always
    /// stays active: the server keeps it so, and the output lists it as kept active.
    Pause {
        /// Boards to pause. Default: every active board.
        boards: Vec<BoardSlug>,
    },
    /// Resume every paused board, or only those named; the daemon dispatches them again.
    Resume {
        /// Boards to resume. Default: every paused board.
        boards: Vec<BoardSlug>,
    },
    /// Show the account's latest usage reading, which daemons report, the boards it stops
    /// from starting new work (`board board update --usage-stop-at`) and the weekly pace.
    Usage {
        /// Keep this weekly percent for your own use and pace board work to the rest
        /// (0 to below 100), or `off`. Only the user may lower it or turn it off.
        #[arg(long, value_parser = parse_reserve)]
        reserve: Option<Reserve>,
        /// Revert the reserve to its earlier value at this time (RFC 3339, YYYY-MM-DD, or
        /// duration ahead such as 2h or 1d). Requires `--reserve`.
        #[arg(long, requires = "reserve", conflicts_with = "until_reset")]
        until: Option<String>,
        /// Revert the reserve at the current usage reading's reset time. Requires `--reserve`.
        #[arg(long, requires = "reserve", conflicts_with = "until")]
        until_reset: bool,
    },
    /// Create or change a board.
    #[command(subcommand)]
    Board(BoardCommand),
    /// Device-level turn limit commands.
    #[command(subcommand)]
    Device(DeviceCommand),
    /// List active devices in the fleet registry with live status, capacity, role, and taints.
    Devices,
    /// List unified runs (managed and interactive) across all devices.
    Runs(RunsArgs),
    /// Show structured live events for a run, or follow them live with -f/--follow.
    Logs(LogsArgs),
    /// Send a steering message to an active run via its owning device's sticky queue.
    Steer {
        /// Run id to steer.
        id: RunId,
        #[arg(help = at_most("Steering message to deliver to the run", RUN_STEER_MESSAGE_CHARS_MAX))]
        message: String,
    },
    /// Request cancellation of a queued or active run.
    Cancel {
        /// Run id to cancel.
        id: RunId,
        #[arg(long, help = at_most("Why the run is being cancelled", RUN_CANCEL_REASON_CHARS_MAX))]
        reason: Option<String>,
    },
    /// Attach to a local run (worktree/T3/tmux/shell) or print SSH and checkout commands for a remote run.
    Attach {
        /// Run id to attach to.
        id: RunId,
        /// Print the resolved attach commands without opening a shell, tmux, or browser link.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the event log.
    Log {
        #[arg(long)]
        board: Option<BoardSlug>,
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Feedback-loop counts (or scheduler telemetry with --scheduler) for the last N days.
    /// Without --scheduler, per-board counts are printed one line per board and in the JSON
    /// `boards` field (`board stats --json`).
    Stats {
        /// Days to count back from now, 1 to 365.
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=365))]
        days: u32,
        /// Show quantitative scheduler telemetry from GET /v1/stats/scheduler.
        #[arg(long)]
        scheduler: bool,
        /// Limit scheduler telemetry to one board (requires --scheduler; without --scheduler,
        /// per-board counts are in the per-board lines and JSON `boards` field).
        #[arg(long, requires = "scheduler")]
        board: Option<BoardSlug>,
        /// Limit scheduler telemetry to one policy_version cohort (requires --scheduler).
        #[arg(long, requires = "scheduler")]
        policy_version: Option<String>,
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
    /// Resolve a feedback item you own or an open one nobody owns (or as the user); it moves to done.
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
    /// Run the remote Streamable HTTP fleet-ops MCP + OAuth 2.1 gateway.
    McpHttp {
        /// Gateway configuration (TOML; see docs/DEPLOY.md).
        #[arg(long, env = "BOARD_MCP_HTTP_CONFIG")]
        config: std::path::PathBuf,
    },
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
    /// Print the brief the daemon starts a worker or a judge of the board with.
    Brief {
        board: BoardSlug,
        /// Which brief: a worker's, or a judge's.
        #[arg(long, value_enum)]
        kind: BriefKind,
        /// The task the brief is for: exactly one for a worker, at least one for a review,
        /// verdict or triage judge, optionally one (the last finished task) for a goal judge,
        /// none for curate. Repeat it for more.
        #[arg(long = "task")]
        tasks: Vec<TaskId>,
    },
    /// Run the device daemon: dispatch (workers, judges, merges) and wake-ups.
    Watch {
        /// Run one reconcile pass and exit.
        #[arg(long)]
        once: bool,
    },
    /// Collect and report this host's health telemetry and scheduled jobs (`[telemetry]` and `[schedules]`).
    ReportHost {
        /// Optional standalone TOML config file (defaults to `$BOARD_HOME/config.toml`).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Override the host/device name reported to the server.
        #[arg(long)]
        host: Option<DeviceName>,
        /// Collect locally and print the JSON payload without calling the server.
        #[arg(long)]
        dry_run: bool,
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

/// [`enum_arg`] that omits `excluded` values completely (used for open enums' `unknown`).
fn enum_arg_excluding<T>(excluded: &[&str]) -> impl clap::builder::TypedValueParser<Value = T>
where
    T: serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    let values: Vec<&str> = ops::enum_values::<T>()
        .iter()
        .copied()
        .filter(|v| !excluded.contains(v))
        .collect();
    debug_assert!(
        !values.is_empty() && values.iter().all(|v| parse_enum::<T>(v).is_ok()),
        "{} is not a unit-variant enum with derived Deserialize",
        std::any::type_name::<T>()
    );
    clap::builder::PossibleValuesParser::new(values)
        .map(|value| parse_enum::<T>(&value).expect("a listed value parses"))
}

#[derive(Debug, Args)]
pub struct RunsArgs {
    /// Only runs on this board.
    #[arg(long)]
    board: Option<BoardSlug>,
    /// Only runs for this task.
    #[arg(long)]
    task: Option<TaskId>,
    /// Only runs on this device.
    #[arg(long)]
    device: Option<DeviceName>,
    /// Only runs of this kind.
    #[arg(long, value_parser = enum_arg_excluding::<RunKind>(&["unknown"]))]
    kind: Option<RunKind>,
    /// Only runs with this origin (`managed` or `interactive`).
    #[arg(long, value_parser = enum_arg_excluding::<RunOrigin>(&["unknown"]))]
    origin: Option<RunOrigin>,
    /// Only runs in this state.
    #[arg(long, value_parser = enum_arg_excluding::<RunState>(&["unknown"]))]
    state: Option<RunState>,
    /// Only non-terminal active runs (`queued`, `leased`, `starting`, `running`, `waiting_external`).
    #[arg(long)]
    active: bool,
    /// Maximum runs to return (1 to 500).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(RUNS_LIMIT_MAX)))]
    limit: Option<u32>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Run id whose structured events to read.
    id: RunId,
    /// Tail live events over SSE until Ctrl-C or the run finishes.
    #[arg(short = 'f', long)]
    follow: bool,
    /// Return only events with `seq` strictly greater than this value.
    #[arg(long)]
    after_seq: Option<i64>,
    /// Maximum events to return in non-follow mode (1 to 200).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(RUN_EVENTS_LIMIT_MAX)))]
    limit: Option<u32>,
}

/// The briefs `board brief` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BriefKind {
    Worker,
    Review,
    AfterMerge,
    Verdict,
    Goal,
    Curate,
    Triage,
}

/// The brief `board brief` prints for `kind` on `board`, or what to pass instead.
fn brief(board: &BoardSlug, kind: BriefKind, tasks: &[TaskId]) -> Result<String> {
    use crate::brief::{JudgeKind, judge_brief, worker_brief};
    let judge = match kind {
        BriefKind::Worker => {
            let [task] = tasks else {
                bail!("a worker brief is for one task: pass --task <id> once");
            };
            return Ok(worker_brief(board, task, crate::brief::WORKER_CLOSER));
        }
        BriefKind::Review => JudgeKind::Review,
        BriefKind::AfterMerge => JudgeKind::AfterMerge,
        BriefKind::Verdict => JudgeKind::Verdict,
        BriefKind::Goal => JudgeKind::Goal,
        BriefKind::Curate => JudgeKind::Curate,
        BriefKind::Triage => JudgeKind::Triage,
    };
    if judge == JudgeKind::Goal {
        if tasks.len() > 1 {
            bail!(
                "a goal judge brief names at most one task (the last finished task): pass --task <id> at most once"
            );
        }
        return Ok(judge_brief(judge, board, tasks));
    }
    if judge.takes_tasks() && tasks.is_empty() {
        bail!(
            "a {} judge brief names its tasks: pass --task <id>, once per task",
            judge.as_str()
        );
    }
    if !judge.takes_tasks() && !tasks.is_empty() {
        bail!(
            "a {} judge brief names no task: leave out --task",
            judge.as_str()
        );
    }
    Ok(judge_brief(judge, board, tasks))
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RuntimeArg {
    Claude,
    Codex,
    /// The opencode board plugin, which sends and reads Claude Code's hook JSON.
    Opencode,
    /// Google Antigravity (Gemini 4 Neon) via T3 Code or standalone CLI.
    Antigravity,
}

impl From<RuntimeArg> for board_proto::Runtime {
    fn from(value: RuntimeArg) -> Self {
        match value {
            RuntimeArg::Claude => Self::Claude,
            RuntimeArg::Codex => Self::Codex,
            RuntimeArg::Opencode => Self::Opencode,
            RuntimeArg::Antigravity => Self::Antigravity,
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
    /// Leave out the done and dropped tasks.
    #[arg(long, conflicts_with = "status")]
    open: bool,
    /// List Icebox (cooled) tasks instead of active ones.
    #[arg(long)]
    icebox: bool,
    /// Only feedback items that still take reports, whatever their status: unresolved and
    /// not closed, declined, or routed to (or a duplicate of) work that is not closed.
    #[arg(long)]
    live: bool,
    /// Only tasks whose id, title, body, acceptance checks or notes hold every one of
    /// these words (at most 8, 200 characters), in any letter case, best match first:
    /// `--search "usage cap"`. When no task holds every word, lists those with any.
    /// Use it instead of listing a whole board as JSON to filter it.
    #[arg(long, visible_alias = "grep")]
    search: Option<String>,
    /// Only tasks changed after this point: an event sequence number (the snapshot's
    /// `seq`), an RFC 3339 time, a date, or a duration back from now (`30m`, `12h`, `2d`).
    #[arg(long)]
    since: Option<String>,
    /// Continue a cut list: only the tasks that follow this one in list order.
    #[arg(long)]
    after: Option<TaskId>,
    /// How many tasks to list at most (default 200, at most 1000).
    #[arg(long)]
    limit: Option<u32>,
    /// With `--json`, print only these fields of each task: `--fields id,title,status`.
    /// Use it instead of piping whole tasks to a filter.
    #[arg(long, value_delimiter = ',')]
    fields: Option<Vec<String>>,
    /// Include bodies and acceptance checks.
    #[arg(long)]
    detailed: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    #[arg(help = at_most(
        &format!(
            "Imperative title, no status or ids; put detail in --body. Aim for about {TITLE_AIM_CHARS} characters"
        ),
        limits().title_chars,
    ))]
    title: String,
    #[arg(long)]
    board: Option<BoardSlug>,
    /// The repository whose board gets the task (its URL or key), in place of --board.
    #[arg(long, conflicts_with = "board")]
    repo: Option<String>,
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
    /// Hard capability predicate a runner device must advertise (e.g. custody:vps-operator);
    /// repeat for more.
    #[arg(long = "requires", value_name = "CAPABILITY")]
    requires: Vec<String>,
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
    /// Place at "top" (rank 1, shifting existing ranks down) or a whole number inside its priority level.
    #[arg(long)]
    rank: Option<String>,
    /// Place directly before this task inside the same priority level.
    #[arg(long)]
    before: Option<TaskId>,
    /// Place directly after this task inside the same priority level.
    #[arg(long)]
    after: Option<TaskId>,
    /// Task size estimate for WSJF ordering (xs, s, m, l).
    #[arg(long, value_parser = enum_arg_hiding::<board_proto::TaskSize>(&["unknown"]))]
    size: Option<board_proto::TaskSize>,
    /// Task origin (user, agent, feedback). User origin defaults to rank 1 when unranked.
    #[arg(long, value_parser = enum_arg_hiding::<board_proto::TaskOrigin>(&["unknown"]))]
    origin: Option<board_proto::TaskOrigin>,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    id: TaskId,
    /// Done goes through `board done`, with evidence.
    #[arg(long, value_parser = enum_arg_hiding::<TaskStatus>(&["done"]))]
    status: Option<TaskStatus>,
    #[arg(long, help = at_most(
        &format!("New title. Aim for about {TITLE_AIM_CHARS} characters"),
        limits().title_chars,
    ))]
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
    /// Replace the task's required runner capabilities (e.g. custody:vps-operator);
    /// repeat for more. Pass `--clear-requires` (or `--requires ""`) to clear them.
    #[arg(
        long = "requires",
        value_name = "CAPABILITY",
        conflicts_with = "clear_requires"
    )]
    requires: Vec<String>,
    /// Clear all required runner capabilities from the task.
    #[arg(long, conflicts_with = "requires")]
    clear_requires: bool,
    // With `--status dropped`, why it is dropped, kept as a decision note (required to drop
    // a task whose owner has left, on a board nobody runs); otherwise why it is blocked.
    #[arg(long, help = at_most(
        "Why it is blocked, only with --status blocked or on a blocked task (leave it out \
         otherwise), or with --status dropped why it is dropped",
        limits().note_chars,
    ))]
    reason: Option<String>,
    /// May the coordinator start it? Set false only when the user must act or decide, or for
    /// a pending proposal; set a proposal true only on an accept verdict.
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    auto: Option<bool>,
    /// Not ready before this date (YYYY-MM-DD, UTC) or RFC 3339 time; none clears it.
    #[arg(long)]
    not_before: Option<String>,
    /// The task's place inside its priority level, lower first; none clears it. `board rank`
    /// orders many tasks in one call.
    #[arg(long)]
    rank: Option<String>,
    /// Hand the task to this agent id (owner, the board's coordinator or the user only, or,
    /// once the owner's session ended, the task's creator or the agent that delegated it).
    #[arg(long)]
    owner: Option<board_proto::AgentId>,
    /// Task size estimate (xs, s, m, l), or none to clear it.
    #[arg(long)]
    size: Option<String>,
    /// Place directly before this task inside the same priority level.
    #[arg(long)]
    before: Option<TaskId>,
    /// Place directly after this task inside the same priority level.
    #[arg(long)]
    after: Option<TaskId>,
    /// Cool an open task into the Icebox (excluded from ready & default lists).
    #[arg(long, conflicts_with = "revive")]
    cool: bool,
    /// Revive an Icebox task back into the active open backlog.
    #[arg(long, conflicts_with = "cool")]
    revive: bool,
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
    /// Close a task in review whose owner is stale, whose managed worker finished or review
    /// passed, or whose owner has left a board that nobody runs (its session ended, or stopped
    /// without an end), in one step and outside your doing limit. Needs a --pr or a --check.
    #[arg(long)]
    take: bool,
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
        /// A device allowed to dispatch the board, in order of preference.
        #[arg(long = "home-device")]
        home_devices: Vec<DeviceName>,
        /// For a test or a one-off: the server closes the board once idle (a day by
        /// default; never while paused) and deletes it later (a week after closing by
        /// default).
        #[arg(long)]
        temporary: bool,
    },
    /// Change a board (also `board board set`).
    #[command(visible_alias = "set")]
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
        /// Whether the server closes the board once idle (never while paused) and deletes
        /// it later. Only the user may turn it on for an existing board.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        temporary: Option<bool>,
        /// How many tasks the board may have in progress at
        /// once (1 to 4). Only the user may raise it.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(board_proto::PARALLEL_TASKS_MAX)))]
        parallel_tasks: Option<u32>,
        /// The usage percent (1 to 100) of the 5-hour or weekly window at which the board
        /// starts no new task. Only the user may raise it.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        usage_stop_at: Option<u32>,
        /// Whether a home device daemon starts automatic verdict judges for pending proposals
        /// when autopilot is off.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        verdict_judges: Option<bool>,
        /// Revert `parallel_tasks` / `usage_stop_at` to their earlier values at this time
        /// (RFC 3339, YYYY-MM-DD, or duration ahead such as 2h or 1d).
        #[arg(long, conflicts_with = "until_reset")]
        until: Option<String>,
        /// Revert `parallel_tasks` / `usage_stop_at` at the current usage reading's reset time.
        #[arg(long, conflicts_with = "until")]
        until_reset: bool,
        /// Minimum gap between goal reviews (e.g. 6h, 30m, 3600s, 1d; 1m to 7d), "0" to disable, or "none" to clear.
        #[arg(long)]
        goal_review_every: Option<String>,
        /// Board kind (project or inbox). Inbox boards disable goal review.
        #[arg(long, value_parser = enum_arg_hiding::<board_proto::BoardKind>(&["unknown"]))]
        kind: Option<board_proto::BoardKind>,
        /// Minimum gap between curate judge passes (e.g. 1d, 24h; 1h to 30d), "0" to disable, or "none" to clear.
        #[arg(long)]
        curate_every: Option<String>,
    },
    /// Delete a closed board without open asks, with its tasks, notes, asks and history.
    /// User tokens only; it cannot be undone.
    Delete { slug: BoardSlug },
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// Wait until this device has enough free agent-turn slots for a launch, counted the
    /// same way as the launch hook, or exit nonzero at the timeout.
    Wait(DeviceWaitArgs),
}

#[derive(Debug, Args)]
pub struct DeviceWaitArgs {
    /// How long to wait before exiting nonzero: seconds such as 300, or a duration such
    /// as 30s or 5m (0 checks once without waiting).
    #[arg(long, default_value = "300", value_parser = parse_timeout)]
    pub timeout: std::time::Duration,
    /// How many free agent-turn slots to wait for.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..).map(|n| n as usize))]
    pub slots: usize,
}

#[derive(Debug, Subcommand)]
pub enum ApprovedCommand {
    /// A board's settings that only the user may raise or turn on.
    #[command(subcommand)]
    Board(ApprovedBoardCommand),
    /// Lower or turn off the account's weekly usage reserve, as the user approved.
    Usage {
        /// Keep this weekly percent for the user (0 to below 100), or `off`.
        #[arg(long, value_parser = parse_reserve)]
        reserve: Reserve,
        /// Revert the reserve to its earlier value at this time (RFC 3339, YYYY-MM-DD, or
        /// duration ahead such as 2h or 1d).
        #[arg(long, conflicts_with = "until_reset")]
        until: Option<String>,
        /// Revert the reserve at the current usage reading's reset time.
        #[arg(long, conflicts_with = "until")]
        until_reset: bool,
        #[arg(long, help = at_most(
            "The user's words that approve this change, quoted",
            board_proto::APPROVAL_SAID_CHARS_MAX,
        ))]
        said: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ApprovedBoardCommand {
    /// Apply the approved settings to a board. It takes only these settings: change any
    /// other with `board board update`.
    Update {
        slug: BoardSlug,
        /// How many tasks the board may have in progress at once (1 to 4).
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(board_proto::PARALLEL_TASKS_MAX)))]
        parallel_tasks: Option<u32>,
        /// The usage percent (1 to 100) at which the board starts no new task.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
        usage_stop_at: Option<u32>,
        /// Whether the server closes the board once idle and deletes it later.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        temporary: Option<bool>,
        /// Revert `parallel_tasks` / `usage_stop_at` to their earlier values at this time
        /// (RFC 3339, YYYY-MM-DD, or duration ahead such as 2h or 1d).
        #[arg(long, conflicts_with = "until_reset")]
        until: Option<String>,
        /// Revert `parallel_tasks` / `usage_stop_at` at the current usage reading's reset time.
        #[arg(long, conflicts_with = "until")]
        until_reset: bool,
        #[arg(long, help = at_most(
            "The user's words that approve this change, quoted",
            board_proto::APPROVAL_SAID_CHARS_MAX,
        ))]
        said: String,
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
    /// Update a device's plane governance metadata over HTTP (requires a --user token).
    DeviceUpdate {
        name: DeviceName,
        /// Fleet plane role (`control`, `runner`, `service_host`).
        #[arg(long, value_enum)]
        role: Option<DeviceRoleArg>,
        /// Scheduling preference weight (-1000 to 1000; higher is preferred).
        #[arg(long, allow_negative_numbers = true, value_parser = clap::value_parser!(i32).range(-1000..=1000))]
        preference: Option<i32>,
        /// Stop scheduling new work on this device when true.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        drain: Option<bool>,
        /// Replace the device's taints (`key[=value]:no_schedule|prefer_no_schedule`); repeat for more.
        #[arg(long = "taint", value_parser = parse_taint, conflicts_with = "clear_taints")]
        taints: Vec<Taint>,
        /// Remove all taints from the device.
        #[arg(long)]
        clear_taints: bool,
    },
    /// Deterministically replay historical tasks/runs from a read-only SQLite database
    /// under the active baseline policy and an optional candidate policy file (TOML or JSON).
    ReplaySchedule {
        /// The server's database file (opened strictly read-only).
        #[arg(long, env = "BOARD_DB")]
        db: std::path::PathBuf,
        /// Candidate policy file (TOML or JSON) to compare against the active baseline policy.
        #[arg(long)]
        policy: Option<std::path::PathBuf>,
        /// Days to replay back from now, 1 to 365 (ignored when --since is passed).
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=365))]
        days: u32,
        /// Replay start time: an RFC 3339 timestamp, date (YYYY-MM-DD), or duration back (e.g. 7d, 48h).
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DeviceRoleArg {
    Control,
    Runner,
    #[value(name = "service_host")]
    ServiceHost,
}

impl From<DeviceRoleArg> for DeviceRole {
    fn from(value: DeviceRoleArg) -> Self {
        match value {
            DeviceRoleArg::Control => Self::Control,
            DeviceRoleArg::Runner => Self::Runner,
            DeviceRoleArg::ServiceHost => Self::ServiceHost,
        }
    }
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

fn admin(cmd: AdminCommand, json: bool) -> anyhow::Result<()> {
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
        AdminCommand::ReplaySchedule {
            db,
            policy,
            days,
            since,
        } => {
            let now = chrono::Utc::now();
            let since = since
                .as_deref()
                .map(|s| ops::parse_since(s, now))
                .transpose()?;
            let candidate = policy
                .as_deref()
                .map(board_server::load_policy_config)
                .transpose()?;
            let cmp = board_server::replay_schedule(
                &db,
                board_server::ReplayOptions {
                    since,
                    until: now,
                    days,
                    candidate,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&cmp)?);
            } else {
                println!("{}", ops::render_replay_comparison(&cmp));
            }
        }
        AdminCommand::DeviceUpdate { .. } => {
            unreachable!("handled over HTTP in dispatch")
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
                // Named as sent: a drop's reason, otherwise the blocked reason.
                let reason = if a.status == Some(TaskStatus::Dropped) {
                    "reason"
                } else {
                    "blocked_reason"
                };
                check_opt(reason, &a.reason, l.note_chars)?;
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
            Self::Ls(a) => check_opt("q", &a.search, board_proto::TASKS_Q_CHARS_MAX)?,
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
            Self::Steer { message, .. } => {
                check_chars(
                    "message",
                    message.trim(),
                    board_proto::RUN_STEER_MESSAGE_CHARS_MAX,
                )?;
            }
            Self::Cancel {
                reason: Some(reason),
                ..
            } => {
                check_chars(
                    "reason",
                    reason.trim(),
                    board_proto::RUN_CANCEL_REASON_CHARS_MAX,
                )?;
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
            Self::Steer { .. } => "POST /v1/runs/{id}/steer",
            Self::Cancel { .. } => "POST /v1/runs/{id}/cancel",
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
                | Self::Rank { .. }
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
        Command::McpHttp { config } => {
            return report(crate::mcp_http::serve(&config).await);
        }
        Command::Admin(cmd) if !matches!(cmd, AdminCommand::DeviceUpdate { .. }) => {
            return report(admin(cmd, cli.json));
        }
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
    if cli.register && cli.agent.is_none() {
        bail!("--register registers the --agent id: pass --agent <id> with it");
    }
    let agent = || agent_from_env(cli.agent.as_deref());
    // --agent naming the identity the environment proves acts as that identity.
    let named_own = std::cell::Cell::new(false);
    let loaded = std::cell::OnceCell::new();
    // The agent's record, once this command has read it.
    let known = std::cell::OnceCell::new();
    let ctx = || -> Result<Ctx> {
        if let Some(ctx) = loaded.get() {
            let mut ctx = Ctx::clone(ctx);
            ctx.known = known.get().cloned();
            return Ok(ctx);
        }
        if let Some(named) = &cli.agent {
            let named: board_proto::AgentId = named.parse().context("--agent")?;
            let own = proven_identity();
            crate::identity::refuse_borrowed(own.as_ref(), &named, "--agent")?;
            named_own.set(own.is_some());
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
        let ctx = ctx()?;
        let named = cli.agent.is_some() && !cli.register && !named_own.get();
        let registered = ctx
            .ensure_registered(named.then_some(ops::NamedAgent::CliFlag))
            .await?;
        if let Some(agent) = registered {
            // Just set: nothing else fills the cell.
            let _ = known.set(agent);
        }
    }
    let out: Output = match cli.command {
        Command::Login { server, device } => return login(&home, server, device),
        Command::Install(cmd) => return crate::install::run(cmd),
        Command::Mcp => return crate::mcp::serve(home, cli.agent, cli.register).await,
        Command::Watch { once } => return crate::daemon::run(home, once).await,
        Command::ReportHost {
            config,
            host,
            dry_run,
        } => {
            return crate::host_report::run_cli(&home, config.as_deref(), host, dry_run, json)
                .await;
        }
        Command::Brief { board, kind, tasks } => {
            println!("{}", brief(&board, kind, &tasks)?);
            return Ok(());
        }
        Command::Device(DeviceCommand::Wait(a)) => device_wait(&home, a).await?,
        Command::Devices => ops::devices(&ctx()?).await?,
        Command::Runs(a) => {
            let filter = RunFilter {
                board: a.board,
                task: a.task,
                device: a.device,
                agent: None,
                kind: a.kind,
                origin: a.origin,
                state: a.state,
                active: a.active.then_some(true),
                limit: a.limit,
            };
            ops::runs(&ctx()?, &filter, chrono::Utc::now()).await?
        }
        Command::Logs(a) => {
            if a.follow {
                return ops::run_logs_follow(&ctx()?, &a.id, a.after_seq, a.limit, json).await;
            }
            ops::run_logs(&ctx()?, &a.id, a.after_seq, a.limit).await?
        }
        Command::Steer { id, message } => ops::steer_run(&ctx()?, &id, message).await?,
        Command::Cancel { id, reason } => ops::cancel_run(&ctx()?, &id, reason).await?,
        Command::Attach { id, dry_run } => ops::attach_run(&ctx()?, &id, dry_run, json).await?,
        Command::Admin(AdminCommand::DeviceUpdate {
            name,
            role,
            preference,
            drain,
            taints,
            clear_taints,
        }) => {
            let taints = if clear_taints {
                Some(Vec::new())
            } else if !taints.is_empty() {
                Some(taints)
            } else {
                None
            };
            let req = AdminUpdateDevice {
                role: role.map(Into::into),
                taints,
                preference,
                drain,
            };
            ops::admin_update_device(&ctx()?, &name, &req).await?
        }
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
                live: a.live.then_some(true),
                closed: a.open.then_some(false),
                icebox: a.icebox.then_some(true),
                q: a.search,
                since: a
                    .since
                    .map(|s| ops::list_since(&s, chrono::Utc::now()))
                    .transpose()?,
                after: a.after,
                limit: a.limit,
            };
            if a.fields.is_some() && !json {
                bail!("--fields picks the fields that --json prints: pass --json with it");
            }
            let ctx = ctx()?;
            if a.mine {
                ctx.agent()?;
            }
            ops::task_list(&ctx, &query, a.detailed, a.fields.as_deref()).await?
        }
        Command::Show {
            ids,
            detailed,
            since,
        } => {
            let since = since
                .map(|s| ops::show_since(&s, chrono::Utc::now()))
                .transpose()?;
            ops::tasks_show(&ctx()?, &ids, detailed, since).await?
        }
        Command::Add(a) => {
            let (rank, rank_top) = ops::parse_add_rank(a.rank.as_deref())?;
            let new = ops::NewTask {
                board: a.board,
                repo: a.repo,
                title: a.title,
                body: a.body,
                acceptance: a.acceptance,
                parent: a.parent,
                blockers: a.blockers,
                requires: a.requires,
                priority: a.priority,
                auto: a.auto,
                not_before: ops::parse_not_before(a.not_before.as_deref())?.flatten(),
                claim: a.claim,
                rank,
                rank_top,
                before: a.before,
                after: a.after,
                origin: a.origin,
                size: a.size,
            };
            ops::task_add(&ctx()?, new).await?
        }
        Command::Claim { id } => ops::task_action(&ctx()?, &id, "claim").await?,
        Command::Release { id } => ops::task_action(&ctx()?, &id, "release").await?,
        Command::Take { id } => ops::task_action(&ctx()?, &id, "take").await?,
        Command::Update(a) => {
            let blockers = a.blockers.map(|list| parse_task_list(&list)).transpose()?;
            let requires = if a.clear_requires {
                Some(Vec::new())
            } else if a.requires.is_empty() {
                None
            } else if a.requires.len() == 1 && a.requires[0].trim().is_empty() {
                Some(Vec::new())
            } else {
                Some(a.requires)
            };
            let (reason, blocked_reason) = if a.status == Some(TaskStatus::Dropped) {
                (a.reason, None)
            } else {
                (None, a.reason)
            };
            let cooled = if a.cool {
                Some(true)
            } else if a.revive {
                Some(false)
            } else {
                None
            };
            let changes = ops::TaskChanges {
                status: a.status,
                title: a.title,
                body: a.body,
                acceptance: a.acceptance,
                priority: a.priority,
                blockers,
                requires,
                blocked_reason,
                auto: a.auto,
                not_before: ops::parse_not_before(a.not_before.as_deref())?,
                rank: ops::parse_rank(a.rank.as_deref())?,
                owner: a.owner,
                reason,
                size: ops::parse_size(a.size.as_deref())?,
                cooled,
                before: a.before,
                after: a.after,
            };
            ops::task_update(&ctx()?, &a.id, changes).await?
        }
        Command::Rank { ids, clear_others } => ops::task_rank(&ctx()?, ids, clear_others).await?,
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
            ops::task_done(&ctx()?, &a.id, evidence, a.summary, a.take).await?
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
            dir,
            ttl_secs,
        } => ops::lease_paths(&ctx()?, globs, task, dir, ttl_secs).await?,
        Command::Ask {
            question,
            board,
            task,
            kind,
            options,
            set_parallel_tasks,
            set_usage_stop_at,
            set_temporary,
        } => {
            let ask = ops::NewAsk {
                board,
                task,
                kind,
                question,
                options,
                change: Some(board_proto::BoardChange {
                    parallel_tasks: set_parallel_tasks,
                    usage_stop_at: set_usage_stop_at,
                    temporary: set_temporary,
                    until: None,
                })
                .filter(|change| !change.is_empty()),
            };
            ops::ask_user(&ctx()?, ask).await?
        }
        Command::Asks {
            status,
            board,
            limit,
        } => {
            let status = (status != "all").then_some(status.as_str());
            ops::asks(&ctx()?, status, board.as_ref(), limit).await?
        }
        Command::Withdraw { id } => ops::ask_withdraw(&ctx()?, id).await?,
        Command::Answer { id, answer } => ops::answer(&ctx()?, id, &answer).await?,
        Command::Join { board } => ops::join(&ctx()?, board).await?,
        Command::Boards { repo } => ops::boards(&ctx()?, repo.as_deref()).await?,
        Command::Pause { boards } => {
            ops::set_boards_status(&ctx()?, boards, BoardStatus::Active, BoardStatus::Paused)
                .await?
        }
        Command::Resume { boards } => {
            ops::set_boards_status(&ctx()?, boards, BoardStatus::Paused, BoardStatus::Active)
                .await?
        }
        Command::Usage {
            reserve,
            until,
            until_reset,
        } => {
            let ctx = ctx()?;
            let now = chrono::Utc::now();
            let until = ops::resolve_until(&ctx, until.as_deref(), until_reset, now).await?;
            ops::usage(&ctx, reserve.map(|r| r.0), until, now).await?
        }
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
            usage_stop_at,
            verdict_judges,
            until,
            until_reset,
            goal_review_every,
            kind,
            curate_every,
        }) => {
            let ctx = ctx()?;
            let until =
                ops::resolve_until(&ctx, until.as_deref(), until_reset, chrono::Utc::now()).await?;
            let body = UpdateBoard {
                title,
                goal,
                status,
                repos,
                autopilot,
                home_devices,
                temporary,
                parallel_tasks,
                dispatch: None,
                usage_stop_at,
                verdict_judges,
                until,
                goal_review_every_secs: ops::parse_goal_review_every(goal_review_every.as_deref())?,
                kind: ops::parse_board_kind(kind)?,
                curate_every_secs: ops::parse_curate_every(curate_every.as_deref())?,
            };
            ops::board_update(&ctx, &slug, body).await?
        }
        Command::Board(BoardCommand::Delete { slug }) => ops::board_delete(&ctx()?, &slug).await?,
        Command::Approved(ApprovedCommand::Board(ApprovedBoardCommand::Update {
            slug,
            parallel_tasks,
            usage_stop_at,
            temporary,
            until,
            until_reset,
            said,
        })) => {
            // The user's login, not the session's device token; the agent is recorded.
            let ctx = Ctx::load_user_login(agent()?)?;
            let until =
                ops::resolve_until(&ctx, until.as_deref(), until_reset, chrono::Utc::now()).await?;
            let change = board_proto::BoardChange {
                parallel_tasks,
                usage_stop_at,
                temporary,
                until,
            };
            ops::board_approved(&ctx, &slug, change, said).await?
        }
        Command::Approved(ApprovedCommand::Usage {
            reserve,
            until,
            until_reset,
            said,
        }) => {
            // The user's login, not the session's device token; the agent is recorded.
            let ctx = Ctx::load_user_login(agent()?)?;
            let until =
                ops::resolve_until(&ctx, until.as_deref(), until_reset, chrono::Utc::now()).await?;
            ops::usage_approved(&ctx, reserve.0, until, said).await?
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
        Command::Stats {
            days,
            scheduler,
            board,
            policy_version,
        } => {
            if scheduler {
                ops::scheduler_stats(&ctx()?, days, board.as_ref(), policy_version.as_deref())
                    .await?
            } else {
                ops::stats(&ctx()?, days, chrono::Utc::now()).await?
            }
        }
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
        | Command::McpHttp { .. }
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

/// A `--reserve` value: a percent, or `off` (`None`).
#[derive(Debug, Clone, Copy)]
pub struct Reserve(pub Option<f64>);

fn parse_reserve(value: &str) -> Result<Reserve> {
    if value == "off" {
        return Ok(Reserve(None));
    }
    let n: f64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("expected a percent such as 5.5, or off"))?;
    if !board_proto::UsagePace::valid_reserve(n) {
        anyhow::bail!("expected a percent from 0 to below 100, or off");
    }
    Ok(Reserve(Some(n)))
}

pub(crate) fn parse_taint(spec: &str) -> Result<Taint> {
    let (kv, effect) = spec.rsplit_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "taint {spec:?} must be key[=value]:effect (effect: no_schedule or prefer_no_schedule)"
        )
    })?;
    let effect = match effect {
        "no_schedule" => TaintEffect::NoSchedule,
        "prefer_no_schedule" => TaintEffect::PreferNoSchedule,
        _ => bail!("unknown taint effect {effect:?}: expected no_schedule or prefer_no_schedule"),
    };
    let (key, value) = match kv.split_once('=') {
        Some((k, v)) if !v.is_empty() => (k, Some(v.to_owned())),
        Some(_) => bail!("taint {spec:?} has an empty value after '='"),
        None => (kv, None),
    };
    if key.is_empty() {
        bail!("taint {spec:?} has an empty key");
    }
    Ok(Taint {
        key: key.to_owned(),
        value,
        effect,
    })
}

fn parse_timeout(value: &str) -> Result<std::time::Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("expected seconds such as 300, or a duration such as 30s or 5m");
    }
    let (digits, mult) = if let Some(n) = trimmed.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = trimmed.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = trimmed.strip_suffix('h') {
        (n, 3600u64)
    } else {
        (trimmed, 1u64)
    };
    let n: u64 = digits.parse().map_err(|_| {
        anyhow::anyhow!("expected seconds such as 300, or a duration such as 30s or 5m")
    })?;
    let secs = n
        .checked_mul(mult)
        .filter(|&s| s <= crate::daemon::device::MAX_WAIT_TIMEOUT.as_secs())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "timeout {value:?} is too large; pass at most {}s (24h)",
                crate::daemon::device::MAX_WAIT_TIMEOUT.as_secs()
            )
        })?;
    Ok(std::time::Duration::from_secs(secs))
}

fn device_wait_poll() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var(crate::daemon::device::WAIT_POLL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
    {
        return std::time::Duration::from_millis(ms);
    }
    crate::daemon::device::DEFAULT_WAIT_POLL
}

async fn device_wait(home: &Home, args: DeviceWaitArgs) -> Result<Output> {
    use crate::daemon::device;
    let config = Config::load(home)?;
    let limit = device::effective_run_limit(&config);
    let t3 = device::t3_cli(&config);
    let ready = device::wait_for_slot(
        &t3,
        &config,
        limit,
        device::WaitOptions {
            slots: args.slots,
            timeout: args.timeout,
            poll: device_wait_poll(),
            count_budget: device::COUNT_BUDGET,
        },
    )
    .await?;
    Ok(Output {
        text: ready.summary(),
        json: serde_json::to_value(&ready)?,
    })
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
    fn claim_help_states_pending_proposals_cannot_be_claimed_and_names_accept_route() {
        let mut cmd = command();
        let err = cmd
            .try_get_matches_from_mut(["board", "claim", "--help"])
            .unwrap_err();
        let help = err.to_string();
        assert!(help.contains("Pending proposals"), "{help}");
        assert!(help.contains("proposals to review"), "{help}");
        assert!(help.contains("cannot be claimed"), "{help}");
        assert!(help.contains("verdict:"), "{help}");
        assert!(help.contains("board update <id> --auto true"), "{help}");
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

    #[test]
    fn device_wait_parses_seconds_and_durations_and_rejects_zero_slots() {
        assert_eq!(parse_timeout("0").unwrap(), std::time::Duration::ZERO);
        assert_eq!(
            parse_timeout("300").unwrap(),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_timeout("30s").unwrap(),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            parse_timeout("5m").unwrap(),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_timeout("1h").unwrap(),
            std::time::Duration::from_secs(3600)
        );
        for bad in [
            "",
            "-1",
            "abc",
            "5d",
            "100000",
            "25h",
            "18446744073709551615m",
        ] {
            assert!(parse_timeout(bad).is_err(), "{bad:?}");
        }

        let cli = Cli::try_parse_from([
            "board",
            "device",
            "wait",
            "--timeout",
            "120",
            "--slots",
            "2",
        ])
        .unwrap();
        let Command::Device(DeviceCommand::Wait(args)) = cli.command else {
            panic!("expected device wait");
        };
        assert_eq!(args.timeout, std::time::Duration::from_secs(120));
        assert_eq!(args.slots, 2);

        // --slots 0 is refused by clap.
        assert!(Cli::try_parse_from(["board", "device", "wait", "--slots", "0"]).is_err());
    }

    #[test]
    fn device_admin_update_and_devices_commands_parse_cleanly() {
        let cli = Cli::try_parse_from(["board", "devices", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Devices));

        let cli = Cli::try_parse_from([
            "board",
            "admin",
            "device-update",
            "control-host",
            "--role",
            "control",
            "--preference",
            "-10",
            "--drain",
            "true",
            "--taint",
            "role=control:no_schedule",
            "--taint",
            "dedicated:prefer_no_schedule",
        ])
        .unwrap();
        let Command::Admin(AdminCommand::DeviceUpdate {
            name,
            role,
            preference,
            drain,
            taints,
            clear_taints,
        }) = cli.command
        else {
            panic!("expected admin device-update");
        };
        assert_eq!(name.as_str(), "control-host");
        assert!(matches!(role, Some(DeviceRoleArg::Control)));
        assert_eq!(preference, Some(-10));
        assert_eq!(drain, Some(true));
        assert!(!clear_taints);
        assert_eq!(
            taints,
            vec![
                Taint {
                    key: "role".into(),
                    value: Some("control".into()),
                    effect: TaintEffect::NoSchedule,
                },
                Taint {
                    key: "dedicated".into(),
                    value: None,
                    effect: TaintEffect::PreferNoSchedule,
                },
            ]
        );
        // Invalid taint specs are refused.
        for bad in [
            "missing-effect",
            "k:bad_effect",
            "=v:no_schedule",
            "k=:no_schedule",
            ":no_schedule",
        ] {
            assert!(parse_taint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scheduler_stats_and_replay_schedule_commands_parse_cleanly() {
        let cli = Cli::try_parse_from([
            "board",
            "stats",
            "--scheduler",
            "--days",
            "14",
            "--board",
            "factory",
            "--policy-version",
            "v1-default",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        let Command::Stats {
            days,
            scheduler,
            board,
            policy_version,
        } = cli.command
        else {
            panic!("expected stats");
        };
        assert_eq!(days, 14);
        assert!(scheduler);
        assert_eq!(board.as_ref().map(BoardSlug::as_str), Some("factory"));
        assert_eq!(policy_version.as_deref(), Some("v1-default"));

        // --board without --scheduler is refused.
        assert!(Cli::try_parse_from(["board", "stats", "--board", "factory"]).is_err());

        // `board stats --help` explains how to get one board's counts (JSON `boards` field
        // and `--board` with `--scheduler`).
        let mut cmd = Cli::command();
        let stats_help = cmd
            .find_subcommand_mut("stats")
            .expect("stats subcommand")
            .render_long_help()
            .to_string();
        assert!(stats_help.contains("--board"), "{stats_help}");
        assert!(stats_help.contains("boards"), "{stats_help}");

        // `board brief --kind goal` accepts 0 or 1 `--task` (naming the last finished task)
        // and refuses more than one.
        let board: BoardSlug = "factory".parse().unwrap();
        let t1: TaskId = "factory-42".parse().unwrap();
        let t2: TaskId = "factory-43".parse().unwrap();
        let goal_no_task = brief(&board, BriefKind::Goal, &[]).unwrap();
        assert!(
            !goal_no_task.contains("Last finished task:"),
            "{goal_no_task}"
        );
        let goal_one_task = brief(&board, BriefKind::Goal, std::slice::from_ref(&t1)).unwrap();
        assert!(
            goal_one_task.contains("Last finished task: factory-42"),
            "{goal_one_task}"
        );
        assert!(brief(&board, BriefKind::Goal, &[t1, t2]).is_err());

        let cli = Cli::try_parse_from([
            "board",
            "admin",
            "replay-schedule",
            "--db",
            "/tmp/board.db",
            "--policy",
            "/tmp/candidate.toml",
            "--days",
            "14",
            "--since",
            "7d",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        let Command::Admin(AdminCommand::ReplaySchedule {
            db,
            policy,
            days,
            since,
        }) = cli.command
        else {
            panic!("expected admin replay-schedule");
        };
        assert_eq!(db, PathBuf::from("/tmp/board.db"));
        assert_eq!(policy, Some(PathBuf::from("/tmp/candidate.toml")));
        assert_eq!(days, 14);
        assert_eq!(since.as_deref(), Some("7d"));
    }

    #[test]
    fn cockpit_runs_logs_steer_cancel_attach_commands_parse_cleanly() {
        let cli = Cli::try_parse_from([
            "board",
            "runs",
            "--board",
            "factory",
            "--task",
            "factory-12",
            "--device",
            "mac-pro",
            "--kind",
            "worker",
            "--origin",
            "managed",
            "--state",
            "running",
            "--active",
            "--limit",
            "50",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        let Command::Runs(args) = cli.command else {
            panic!("expected runs");
        };
        assert_eq!(args.board.as_ref().map(BoardSlug::as_str), Some("factory"));
        assert_eq!(args.task.as_ref().map(TaskId::as_str), Some("factory-12"));
        assert_eq!(
            args.device.as_ref().map(DeviceName::as_str),
            Some("mac-pro")
        );
        assert_eq!(args.kind, Some(RunKind::Worker));
        assert_eq!(args.origin, Some(RunOrigin::Managed));
        assert_eq!(args.state, Some(RunState::Running));
        assert!(args.active);
        assert_eq!(args.limit, Some(50));

        // Open-enum `unknown` is rejected at CLI parse time.
        assert!(Cli::try_parse_from(["board", "runs", "--kind", "unknown"]).is_err());
        assert!(Cli::try_parse_from(["board", "runs", "--origin", "unknown"]).is_err());
        assert!(Cli::try_parse_from(["board", "runs", "--state", "unknown"]).is_err());

        let cli = Cli::try_parse_from([
            "board",
            "logs",
            "run-01a2b3",
            "-f",
            "--after-seq",
            "5",
            "--limit",
            "100",
        ])
        .unwrap();
        let Command::Logs(logs) = cli.command else {
            panic!("expected logs");
        };
        assert_eq!(logs.id.as_str(), "run-01a2b3");
        assert!(logs.follow);
        assert_eq!(logs.after_seq, Some(5));
        assert_eq!(logs.limit, Some(100));

        let cli = Cli::try_parse_from(["board", "steer", "run-01a2b3", "focus on tests"]).unwrap();
        assert!(matches!(cli.command, Command::Steer { .. }));

        let cli = Cli::try_parse_from(["board", "cancel", "run-01a2b3", "--reason", "superseded"])
            .unwrap();
        assert!(matches!(cli.command, Command::Cancel { .. }));

        let cli = Cli::try_parse_from(["board", "attach", "run-01a2b3", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Command::Attach { dry_run: true, .. }));
    }

    #[test]
    fn add_and_update_requires_flags_parse_and_conflict_cleanly() {
        let cli = Cli::try_parse_from([
            "board",
            "add",
            "Rotate certs",
            "--requires",
            "custody:vps-operator",
            "--requires",
            "os:macos",
        ])
        .unwrap();
        let Command::Add(add) = cli.command else {
            panic!("expected add");
        };
        assert_eq!(add.requires, vec!["custody:vps-operator", "os:macos"]);

        let cli = Cli::try_parse_from([
            "board",
            "update",
            "factory-12",
            "--requires",
            "custody:vps-operator",
            "--requires",
            "os:macos",
        ])
        .unwrap();
        let Command::Update(update) = cli.command else {
            panic!("expected update");
        };
        assert_eq!(update.requires, vec!["custody:vps-operator", "os:macos"]);
        assert!(!update.clear_requires);

        let cli =
            Cli::try_parse_from(["board", "update", "factory-12", "--clear-requires"]).unwrap();
        let Command::Update(update) = cli.command else {
            panic!("expected update");
        };
        assert!(update.requires.is_empty());
        assert!(update.clear_requires);

        let cli = Cli::try_parse_from(["board", "update", "factory-12", "--requires", ""]).unwrap();
        let Command::Update(update) = cli.command else {
            panic!("expected update");
        };
        assert_eq!(update.requires, vec![""]);
        assert!(!update.clear_requires);

        assert!(
            Cli::try_parse_from([
                "board",
                "update",
                "factory-12",
                "--requires",
                "custody:vps-operator",
                "--clear-requires",
            ])
            .is_err()
        );
    }
}
