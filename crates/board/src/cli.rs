//! Command-line interface.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use board_proto::{
    AskKind, BoardSlug, BoardStatus, CreateBoard, DeviceName, NoteKind, Priority, SnapshotFormat,
    SnapshotKind, TaskId, TaskStatus, UpdateBoard,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::client::TaskQuery;
use crate::config::{Config, Home, TokenStore, store_token};
use crate::identity::agent_from_env;
use crate::ops::{self, Ctx, Output, parse_enum};

#[derive(Debug, Parser)]
#[command(
    name = "board",
    version,
    about = "Shared work board for coding-agent threads"
)]
pub struct Cli {
    /// Act as this agent id instead of the one from the environment.
    #[arg(long, global = true)]
    pub agent: Option<String>,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
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
    /// Show a task with its notes and evidence.
    Show {
        id: TaskId,
        #[arg(long)]
        detailed: bool,
    },
    /// Add a task.
    Add(AddArgs),
    /// Claim a task: it moves to doing with you as owner.
    Claim { id: TaskId },
    /// Release a task you own back to open.
    Release { id: TaskId },
    /// Take a task whose owner is stale.
    Take { id: TaskId },
    /// Change a task's fields or status.
    Update(UpdateArgs),
    /// Mark a task done with evidence.
    Done(DoneArgs),
    /// Add a note to a task.
    Note {
        id: TaskId,
        text: String,
        #[arg(long, default_value = "note", value_parser = parse_enum::<NoteKind>)]
        kind: NoteKind,
    },
    /// Show your handoff, or replace it with --now and --next.
    Handoff {
        #[arg(long, requires = "next")]
        now: Option<String>,
        #[arg(long, requires = "now")]
        next: Option<String>,
        #[arg(long, default_value = "")]
        blockers: String,
        /// Show another agent's handoff.
        #[arg(long, conflicts_with = "now")]
        of: Option<board_proto::AgentId>,
    },
    /// Lease files (globs relative to the repo root) to warn peers.
    Lease {
        #[arg(required = true)]
        globs: Vec<String>,
        #[arg(long)]
        task: Option<TaskId>,
        #[arg(long)]
        ttl_secs: Option<u32>,
    },
    /// Ask the user for a decision or an action.
    Ask {
        question: String,
        #[arg(long)]
        board: Option<BoardSlug>,
        #[arg(long)]
        task: Option<TaskId>,
        #[arg(long, default_value = "decision", value_parser = parse_enum::<AskKind>)]
        kind: AskKind,
        /// A suggested answer, recommended first; repeat for more.
        #[arg(long = "option")]
        options: Vec<String>,
    },
    /// List asks.
    Asks {
        #[arg(long, default_value = "open")]
        status: String,
        #[arg(long)]
        board: Option<BoardSlug>,
    },
    /// Answer an ask (user tokens only).
    Answer { id: i64, answer: String },
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
    /// Print your board snapshot.
    Snapshot {
        #[arg(long, default_value = "full", value_parser = parse_enum::<SnapshotKind>)]
        kind: SnapshotKind,
        #[arg(long, default_value = "lines", value_parser = parse_enum::<SnapshotFormat>)]
        format: SnapshotFormat,
        #[arg(long)]
        since: Option<i64>,
    },
    /// File feedback about the board on the feedback board.
    Feedback { text: String },
    /// Run the MCP server on stdio.
    Mcp,
    /// Handle a Claude Code or Codex hook event (reads its JSON from stdin).
    Hook {
        event: crate::hook::HookEvent,
        #[arg(long, value_enum)]
        runtime: RuntimeArg,
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
    #[arg(long)]
    board: Option<BoardSlug>,
    #[arg(long, value_parser = parse_enum::<TaskStatus>)]
    status: Option<TaskStatus>,
    #[arg(long)]
    owner: Option<board_proto::AgentId>,
    #[arg(long)]
    limit: Option<u32>,
    /// Include bodies and acceptance checks.
    #[arg(long)]
    detailed: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    title: String,
    #[arg(long)]
    board: Option<BoardSlug>,
    #[arg(long, default_value = "")]
    body: String,
    /// How done is checked: one observable check per line.
    #[arg(long = "accept", default_value = "")]
    acceptance: String,
    #[arg(long)]
    parent: Option<TaskId>,
    /// A task that must be done first; repeat for more.
    #[arg(long = "blocker")]
    blockers: Vec<TaskId>,
    #[arg(long, value_parser = parse_enum::<Priority>)]
    priority: Option<Priority>,
    /// May the coordinator start it without the user? Default: the board's autopilot.
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    auto: Option<bool>,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    id: TaskId,
    #[arg(long, value_parser = parse_enum::<TaskStatus>)]
    status: Option<TaskStatus>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long = "accept")]
    acceptance: Option<String>,
    #[arg(long, value_parser = parse_enum::<Priority>)]
    priority: Option<Priority>,
    /// Comma-separated blockers; an empty value clears them.
    #[arg(long)]
    blockers: Option<String>,
    #[arg(long = "reason")]
    blocked_reason: Option<String>,
    #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
    auto: Option<bool>,
}

#[derive(Debug, Args)]
pub struct DoneArgs {
    id: TaskId,
    /// A check command that was run; pair each with --exit.
    #[arg(long = "check")]
    checks: Vec<String>,
    /// Exit code of the matching --check.
    #[arg(long = "exit", allow_negative_numbers = true)]
    exits: Vec<i32>,
    #[arg(long = "commit")]
    commits: Vec<String>,
    #[arg(long = "pr")]
    prs: Vec<String>,
    #[arg(long = "artifact")]
    artifacts: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum BoardCommand {
    /// Create a board.
    Create {
        slug: BoardSlug,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        goal: String,
        /// A repo key such as github.com/owner/repo; repeat for more.
        #[arg(long = "repo")]
        repos: Vec<String>,
        #[arg(long)]
        autopilot: bool,
        /// A device allowed to host the coordinator, in order of preference.
        #[arg(long = "home-device")]
        home_devices: Vec<DeviceName>,
    },
    /// Change a board.
    Update {
        slug: BoardSlug,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        goal: Option<String>,
        #[arg(long, value_parser = parse_enum::<BoardStatus>)]
        status: Option<BoardStatus>,
        /// Replace the repos; repeat for more.
        #[arg(long = "repo")]
        repos: Option<Vec<String>>,
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        autopilot: Option<bool>,
        /// Replace the home devices; repeat for more.
        #[arg(long = "home-device")]
        home_devices: Option<Vec<DeviceName>>,
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
    let config = board_server::ServerConfig::load(path)?;
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
        )
    }
}

/// Runs the CLI and returns the process exit code.
pub async fn run(cli: Cli) -> i32 {
    match cli.command {
        // Hooks never fail the turn.
        Command::Hook { event, runtime } => {
            crate::hook::run(event, runtime.into(), cli.agent.as_deref()).await;
            return 0;
        }
        Command::Serve { config } => {
            return report(serve(&config).await);
        }
        Command::Admin(cmd) => return report(admin(cmd)),
        _ => {}
    }
    match dispatch(cli).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("board: {err:#}");
            1
        }
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
        let ctx = Ctx::load(home.clone(), agent()?, None)?;
        Ok(loaded.get_or_init(|| ctx).clone())
    };
    if cli.command.acts_as_agent() {
        ctx()?.ensure_registered().await?;
    }
    let out: Output = match cli.command {
        Command::Login { server, device } => return login(&home, server, device),
        Command::Install(cmd) => return crate::install::run(cmd),
        Command::Mcp => return crate::mcp::serve(home, cli.agent).await,
        Command::Watch { once } => return crate::daemon::run(home, once).await,
        Command::Status { board, detailed } => ops::status(&ctx()?, board, detailed).await?,
        Command::Ls(a) => {
            let query = TaskQuery {
                board: a.board,
                status: a.status,
                owner: a.owner,
                ready: a.ready.then_some(true),
                mine: a.mine.then_some(true),
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
            ops::task_done(&ctx()?, &a.id, evidence).await?
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
        }) => {
            let body = CreateBoard {
                slug,
                title,
                goal,
                repos,
                autopilot,
                home_devices,
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
        }) => {
            let body = UpdateBoard {
                title,
                goal,
                status,
                repos,
                autopilot,
                home_devices,
            };
            ops::board_update(&ctx()?, &slug, body).await?
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
        Command::Feedback { text } => ops::feedback(&ctx()?, text).await?,
        Command::Stats { days } => ops::stats(&ctx()?, days).await?,
        Command::Hook { .. } | Command::Serve { .. } | Command::Admin(_) => {
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
