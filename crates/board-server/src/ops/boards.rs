use board_proto::{
    AgentId, ApprovedBoardChange, Board, BoardChange, BoardKind, BoardSlug, BoardStatus,
    ClaimCoordinator, ConflictKeySpec, CreateBoard, DeviceName, EventType, TaskId, UpdateBoard,
};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeMap;

use crate::config::Settings;
use crate::db::{Ctx, enum_str, live_sql, parse_id};
use crate::error::{Result, bad_transition, forbidden, invalid, refused};
use crate::ops::{expires, join_board, touch_agent};
use crate::store::{agent_missing, board_missing, get_board, get_board_uncounted};
use crate::text::{ago, check_line_or, check_text, normalize_repo};
use board_proto::{
    BOARD_TITLE_LONGER_TEXT_GOES, CURATE_EVERY_SECS_MAX, CURATE_EVERY_SECS_MIN,
    GOAL_REVIEW_EVERY_SECS_MAX, GOAL_REVIEW_EVERY_SECS_MIN, PARALLEL_TASKS_MAX,
};

/// Default cadence in seconds for periodic backlog curation (`RunKind::Curate`): 24 hours.
pub(crate) const CURATE_EVERY_SECS_DEFAULT: u64 = 86_400;

/// Whether goal reviews (`RunKind::Goal`) are disabled on `board`: true for `inbox` boards
/// and when the effective interval (`board.goal_review_every_secs.or(default_secs)`) is `0`.
pub(crate) fn goal_review_disabled(board: &Board, default_secs: Option<u64>) -> bool {
    board.kind == BoardKind::Inbox || board.goal_review_every_secs.or(default_secs) == Some(0)
}

/// Whether periodic curation (`RunKind::Curate`) is disabled on `board`: true for `inbox`
/// boards and for any board with `curate_every_secs == Some(0)`.
pub(crate) fn curate_disabled(board: &Board) -> bool {
    board.kind == BoardKind::Inbox || board.curate_every_secs == Some(0)
}

/// Effective cadence in seconds for periodic backlog curation (`RunKind::Curate`) on
/// `board`: `None` when disabled (`inbox` or `curate_every_secs == Some(0)`), else the
/// configured cadence or [`CURATE_EVERY_SECS_DEFAULT`] (86,400s / 1 day).
pub(crate) fn effective_curate_every_secs(board: &Board) -> Option<u64> {
    if curate_disabled(board) {
        return None;
    }
    Some(board.curate_every_secs.unwrap_or(CURATE_EVERY_SECS_DEFAULT))
}

/// Effective minimum interval in seconds between goal reviews on `board`: per-board
/// `goal_review_every_secs` if set, else `default_secs`, or `None` when disabled or unset.
pub(crate) fn effective_goal_review_every_secs(
    board: &Board,
    _feedback_board: &BoardSlug,
    default_secs: Option<u64>,
) -> Option<u64> {
    if goal_review_disabled(board, default_secs) {
        return None;
    }
    board
        .goal_review_every_secs
        .or(default_secs)
        .filter(|&secs| secs > 0)
}

fn check_repos(repos: Vec<String>) -> Result<Vec<String>> {
    if repos.len() > 20 {
        return Err(invalid(
            "a board covers at most 20 repos",
            "split the work into several boards",
        ));
    }
    let mut out = Vec::new();
    for repo in repos {
        let key = normalize_repo(&repo)?;
        if !out.contains(&key) {
            out.push(key);
        }
    }
    Ok(out)
}

fn check_devices(devices: Vec<DeviceName>) -> Result<Vec<DeviceName>> {
    if devices.len() > 10 {
        return Err(invalid(
            "at most 10 home devices",
            "list the devices that should host the coordinator, in order of preference",
        ));
    }
    let mut out: Vec<DeviceName> = Vec::new();
    for device in devices {
        if !out.contains(&device) {
            out.push(device);
        }
    }
    Ok(out)
}

pub(crate) fn validate_create(mut req: CreateBoard, s: &Settings) -> Result<CreateBoard> {
    check_line_or(
        "title",
        &req.title,
        s.title_chars,
        Some(BOARD_TITLE_LONGER_TEXT_GOES),
    )?;
    check_text("goal", &req.goal, s.body_chars)?;
    req.repos = check_repos(req.repos)?;
    req.home_devices = check_devices(req.home_devices)?;
    Ok(req)
}

pub(crate) fn validate_update(mut req: UpdateBoard, s: &Settings) -> Result<UpdateBoard> {
    if let Some(title) = &req.title {
        check_line_or(
            "title",
            title,
            s.title_chars,
            Some(BOARD_TITLE_LONGER_TEXT_GOES),
        )?;
    }
    if let Some(goal) = &req.goal {
        check_text("goal", goal, s.body_chars)?;
    }
    req.repos = req.repos.map(check_repos).transpose()?;
    req.home_devices = req.home_devices.map(check_devices).transpose()?;
    if let Some(n) = req.parallel_tasks
        && !(1..=PARALLEL_TASKS_MAX).contains(&n)
    {
        return Err(invalid(
            format!("parallel_tasks={n} is out of range"),
            format!("send parallel_tasks between 1 and {PARALLEL_TASKS_MAX}"),
        ));
    }
    if let Some(n) = req.usage_stop_at
        && !(1..=100).contains(&n)
    {
        return Err(invalid(
            format!("usage_stop_at={n} is out of range"),
            "send usage_stop_at between 1 and 100 (percent of a usage window)",
        ));
    }
    if req.until.is_some() && req.parallel_tasks.is_none() && req.usage_stop_at.is_none() {
        return Err(invalid(
            "until applies only to parallel_tasks or usage_stop_at",
            "send parallel_tasks or usage_stop_at with until, or leave until out",
        ));
    }
    if let Some(Some(secs)) = req.goal_review_every_secs
        && secs != 0
        && !(GOAL_REVIEW_EVERY_SECS_MIN..=GOAL_REVIEW_EVERY_SECS_MAX).contains(&secs)
    {
        return Err(invalid(
            format!("goal_review_every_secs={secs} is out of range"),
            format!(
                "send goal_review_every_secs as 0 (disable), between {GOAL_REVIEW_EVERY_SECS_MIN} \
                 and {GOAL_REVIEW_EVERY_SECS_MAX} seconds, or null to clear"
            ),
        ));
    }
    if req.kind == Some(BoardKind::Unknown) {
        return Err(invalid(
            "unknown board kind",
            "send kind as project or inbox",
        ));
    }
    if let Some(Some(secs)) = req.curate_every_secs
        && secs != 0
        && !(CURATE_EVERY_SECS_MIN..=CURATE_EVERY_SECS_MAX).contains(&secs)
    {
        return Err(invalid(
            format!("curate_every_secs={secs} is out of range"),
            format!(
                "send curate_every_secs as 0 (disable), between {CURATE_EVERY_SECS_MIN} and \
                 {CURATE_EVERY_SECS_MAX} seconds, or null to clear"
            ),
        ));
    }
    Ok(req)
}

pub(crate) fn create(cx: &mut Ctx<'_>, actor: &str, req: CreateBoard) -> Result<Board> {
    if cx.exists("SELECT 1 FROM boards WHERE slug = ?1", [req.slug.as_str()])? {
        return Err(invalid(
            format!("board {} already exists", req.slug),
            "use the existing board, or pick another slug",
        ));
    }
    let now = cx.now_ms();
    let is_feedback = req.slug == cx.s.feedback_board;
    let default_kind = if is_feedback {
        BoardKind::Inbox
    } else {
        BoardKind::Project
    };
    let default_goal_every: Option<i64> = is_feedback.then_some(0);
    cx.db.execute(
        // A slug that was deleted before resumes its task numbering.
        "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
         temporary, dispatch, goal_review_every_secs, kind, next_number, created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6, ?7, 'daemon', ?8, ?9, \
         COALESCE((SELECT next_number FROM deleted_boards WHERE slug = ?1), 1), ?10, ?10)",
        params![
            req.slug.as_str(),
            req.title,
            req.goal,
            serde_json::to_string(&req.repos).expect("strings serialize"),
            req.autopilot,
            serde_json::to_string(&req.home_devices).expect("strings serialize"),
            req.temporary,
            default_goal_every,
            enum_str(&default_kind),
            now,
        ],
    )?;
    cx.emit(
        Some(&req.slug),
        actor,
        EventType::BoardCreated,
        None,
        serde_json::json!({
            "title": req.title,
            "repos": req.repos,
            "autopilot": req.autopilot,
            "temporary": req.temporary,
        }),
    )?;
    get_board(cx, &req.slug)
}

/// The two routes by which an agent gets a user-only setting changed, named by each
/// refusal of one.
fn approved_routes(slug: &BoardSlug, setting: &str, flag: &str, value: &str) -> String {
    format!(
        "ask the user with the change attached (ask_user with change {{\"{setting}\": {value}}}; \
         CLI: board ask --set-{flag} {value}): the server applies it when they pick the first \
         option. When the user already approved this change in their own words, run `board \
         approved board update {slug} --{flag} {value} --said \"<their words>\"`"
    )
}

/// How the user approved a change to the settings only they may raise or turn on. The
/// `board_updated` event of the change records it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Approval<'a> {
    /// The user answered ask `id`, asked by `agent`, with its approving option.
    Ask { id: i64, agent: &'a AgentId },
    /// The user said `said`; the caller, on `device`'s user login, quotes it.
    Said {
        said: &'a str,
        device: Option<&'a DeviceName>,
    },
    /// The user authorized answering ask `id` (asked by `agent`) in chat (`said`),
    /// spoken for by the caller on `device`'s user login. Preserves ask-change staleness
    /// semantics while recording both the ask and the user's quoted words.
    AskSaid {
        id: i64,
        agent: &'a AgentId,
        said: &'a str,
        device: Option<&'a DeviceName>,
    },
}

/// Checks a change that goes through an approval: it names a setting, in range.
pub(crate) fn validate_change(change: &BoardChange, s: &Settings) -> Result<()> {
    if change.is_empty() {
        return Err(invalid(
            "the change names no setting",
            "send at least one of parallel_tasks, usage_stop_at and temporary",
        ));
    }
    validate_update(change_request(change), s).map(|_| ())
}

/// Checks the user's quoted words on an approved change.
pub(crate) fn validate_approved(req: &ApprovedBoardChange) -> Result<()> {
    if req.said.trim().is_empty() {
        return Err(invalid(
            "said is empty",
            "quote the user's words that approve this change",
        ));
    }
    check_text("said", &req.said, board_proto::APPROVAL_SAID_CHARS_MAX)
}

fn change_request(change: &BoardChange) -> UpdateBoard {
    UpdateBoard {
        parallel_tasks: change.parallel_tasks,
        usage_stop_at: change.usage_stop_at,
        temporary: change.temporary,
        until: change.until,
        ..UpdateBoard::default()
    }
}

/// Applies a change the user approved, with the user's rights. Always records a
/// `board_updated` event that names the approval and the values, also when the board
/// already had them, so no use of an approval is without a trace.
pub(crate) fn update_approved(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    change: &BoardChange,
    approval: Approval<'_>,
) -> Result<Board> {
    apply(
        cx,
        actor,
        slug,
        change_request(change),
        true,
        Some(approval),
    )
}

/// Changes a board. Only the user may make a board temporary, since the sweep then
/// deletes it once idle.
pub(crate) fn update(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    req: UpdateBoard,
    by_user: bool,
) -> Result<Board> {
    apply(cx, actor, slug, req, by_user, None)
}

fn apply(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    mut req: UpdateBoard,
    by_user: bool,
    approval: Option<Approval<'_>>,
) -> Result<Board> {
    let before = get_board(cx, slug)?;
    // The user keeps the feedback board on whatever other boards do, so triage never stops.
    // A pause of it is dropped, not refused, so an older client's `board pause` of every
    // board goes on to the rest. Closing it, to retire or delete it, is a separate act.
    if *slug == cx.s.feedback_board && req.status == Some(BoardStatus::Paused) {
        req.status = None;
    }
    // Thread dispatch was removed: the value is dropped, not refused, so an older client's
    // update still applies its other fields.
    req.dispatch = None;
    if req.temporary == Some(true) && !before.temporary && !by_user {
        return Err(forbidden(
            format!("only the user may make board {slug} temporary"),
            format!(
                "temporary boards are deleted once idle; create a new board with temporary \
                 set, or {}",
                approved_routes(slug, "temporary", "temporary", "true")
            ),
        ));
    }
    let parallel_baseline = before.parallel_tasks_prev.unwrap_or(before.parallel_tasks);
    if let Some(n) = req.parallel_tasks
        && !by_user
    {
        let exceeds_now = n > before.parallel_tasks;
        let extends_above_baseline = n > parallel_baseline
            && match (req.until, before.parallel_tasks_until) {
                (None, _) => true,
                (Some(u), Some(prev_u)) => u > prev_u,
                (Some(_), None) => true,
            };
        if exceeds_now || extends_above_baseline {
            return Err(forbidden(
                format!("only the user may raise parallel_tasks on board {slug}"),
                format!(
                    "more tasks at once spend the shared usage limit faster; {}",
                    approved_routes(slug, "parallel_tasks", "parallel-tasks", "<n>")
                ),
            ));
        }
    }
    let stop_baseline = before.usage_stop_at_prev.unwrap_or(before.usage_stop_at);
    if let Some(n) = req.usage_stop_at
        && !by_user
    {
        let exceeds_now = n > before.usage_stop_at;
        let extends_above_baseline = n > stop_baseline
            && match (req.until, before.usage_stop_at_until) {
                (None, _) => true,
                (Some(u), Some(prev_u)) => u > prev_u,
                (Some(_), None) => true,
            };
        if exceeds_now || extends_above_baseline {
            return Err(forbidden(
                format!("only the user may raise usage_stop_at on board {slug}"),
                format!(
                    "a higher stop lets the board spend more of the shared usage limit; {}",
                    approved_routes(slug, "usage_stop_at", "usage-stop-at", "<n>")
                ),
            ));
        }
    }
    // An ask may be answered long after it was written. Its answer never lowers a value
    // the board has since been given, and never applies a timed limit whose `until` has
    // already passed: the event names what it left alone.
    let mut not_lowered = Vec::new();
    let mut expired_before_answer = false;
    let is_ask_approval = matches!(
        approval,
        Some(Approval::Ask { .. } | Approval::AskSaid { .. })
    );
    if let Some(until) = req.until
        && until <= cx.now
    {
        if is_ask_approval {
            req.parallel_tasks = None;
            req.usage_stop_at = None;
            req.until = None;
            expired_before_answer = true;
        } else {
            return Err(invalid(
                format!("until {until} is not in the future"),
                "send a future RFC 3339 time, or leave until out for a permanent change",
            ));
        }
    }
    if is_ask_approval {
        if req
            .parallel_tasks
            .is_some_and(|n| n < before.parallel_tasks)
        {
            req.parallel_tasks = None;
            not_lowered.push("parallel_tasks");
        }
        if req.usage_stop_at.is_some_and(|n| n < before.usage_stop_at) {
            req.usage_stop_at = None;
            not_lowered.push("usage_stop_at");
        }
    }
    // Compute timed limit overrides. Extending an already-timed limit keeps the original
    // permanent baseline; setting a limit without `until` clears its timer.
    let (next_parallel_prev, next_parallel_until) = match req.parallel_tasks {
        Some(new_val) => match req.until {
            Some(until) => {
                let baseline = before.parallel_tasks_prev.unwrap_or(before.parallel_tasks);
                if new_val != baseline {
                    (Some(baseline), Some(until))
                } else {
                    (None, None)
                }
            }
            None => (None, None),
        },
        None => (before.parallel_tasks_prev, before.parallel_tasks_until),
    };
    let (next_stop_prev, next_stop_until) = match req.usage_stop_at {
        Some(new_val) => match req.until {
            Some(until) => {
                let baseline = before.usage_stop_at_prev.unwrap_or(before.usage_stop_at);
                if new_val != baseline {
                    (Some(baseline), Some(until))
                } else {
                    (None, None)
                }
            }
            None => (None, None),
        },
        None => (before.usage_stop_at_prev, before.usage_stop_at_until),
    };
    let mut fields = Vec::new();
    let mut set = |field: &'static str, changed: bool| {
        if changed {
            fields.push(field);
        }
    };
    set(
        "title",
        req.title.as_ref().is_some_and(|v| *v != before.title),
    );
    set("goal", req.goal.as_ref().is_some_and(|v| *v != before.goal));
    set("status", req.status.is_some_and(|v| v != before.status));
    set(
        "repos",
        req.repos.as_ref().is_some_and(|v| *v != before.repos),
    );
    set(
        "autopilot",
        req.autopilot.is_some_and(|v| v != before.autopilot),
    );
    set(
        "home_devices",
        req.home_devices
            .as_ref()
            .is_some_and(|v| *v != before.home_devices),
    );
    set(
        "temporary",
        req.temporary.is_some_and(|v| v != before.temporary),
    );
    set(
        "parallel_tasks",
        req.parallel_tasks
            .is_some_and(|v| v != before.parallel_tasks),
    );
    set(
        "parallel_tasks_until",
        next_parallel_until != before.parallel_tasks_until
            || next_parallel_prev != before.parallel_tasks_prev,
    );
    set(
        "dispatch",
        req.dispatch.is_some_and(|v| v != before.dispatch),
    );
    set(
        "usage_stop_at",
        req.usage_stop_at.is_some_and(|v| v != before.usage_stop_at),
    );
    set(
        "usage_stop_at_until",
        next_stop_until != before.usage_stop_at_until
            || next_stop_prev != before.usage_stop_at_prev,
    );
    set(
        "verdict_judges",
        req.verdict_judges
            .is_some_and(|v| v != before.verdict_judges),
    );
    set(
        "goal_review_every_secs",
        req.goal_review_every_secs
            .is_some_and(|v| v != before.goal_review_every_secs),
    );
    set("kind", req.kind.is_some_and(|v| v != before.kind));
    set(
        "curate_every_secs",
        req.curate_every_secs
            .is_some_and(|v| v != before.curate_every_secs),
    );
    if fields.is_empty() && approval.is_none() {
        return Ok(before);
    }
    // A status change names the new status, so a daemon sees a resumed board without
    // reading it.
    let changed = !fields.is_empty();
    let mut payload = serde_json::json!({ "fields": fields });
    if !not_lowered.is_empty() {
        payload["not_lowered"] = serde_json::json!(not_lowered);
    }
    if expired_before_answer {
        payload["expired_before_answer"] = serde_json::Value::from(true);
    }
    if let Some(until) = req.until
        && ((req.parallel_tasks.is_some() && next_parallel_until == Some(until))
            || (req.usage_stop_at.is_some() && next_stop_until == Some(until)))
    {
        payload["until"] = serde_json::to_value(until).expect("timestamp serializes");
    }
    if let Some(status) = req.status.filter(|&s| s != before.status) {
        payload["status"] = serde_json::Value::from(enum_str(&status));
    }
    if let Some(approval) = approval {
        // The values and who approved them, so the event alone shows what was granted.
        for (name, value) in [
            (
                "parallel_tasks",
                req.parallel_tasks.map(serde_json::Value::from),
            ),
            (
                "usage_stop_at",
                req.usage_stop_at.map(serde_json::Value::from),
            ),
            ("temporary", req.temporary.map(serde_json::Value::from)),
        ] {
            if let Some(value) = value {
                payload[name] = value;
            }
        }
        match approval {
            Approval::Ask { id, agent } => {
                payload["approved"] = "ask".into();
                payload["ask"] = id.into();
                payload["agent"] = agent.as_str().into();
            }
            Approval::Said { said, device } => {
                payload["approved"] = "said".into();
                payload["said"] = said.into();
                if let Some(device) = device {
                    payload["device"] = device.as_str().into();
                }
            }
            Approval::AskSaid {
                id,
                agent,
                said,
                device,
            } => {
                payload["approved"] = "said".into();
                payload["ask"] = id.into();
                payload["agent"] = agent.as_str().into();
                payload["said"] = said.into();
                if let Some(device) = device {
                    payload["device"] = device.as_str().into();
                }
            }
        }
    }
    // An approval that changes no value still leaves its event, but not a write: the
    // board's `updated_at` moves only when the board does.
    if changed {
        let new_parallel = req.parallel_tasks.unwrap_or(before.parallel_tasks);
        let next_repos = req.repos.unwrap_or(before.repos);
        let next_goal_every: Option<i64> = req
            .goal_review_every_secs
            .unwrap_or(before.goal_review_every_secs)
            .map(|v| i64::try_from(v).expect("validated goal_review_every_secs fits i64"));
        let next_kind = req.kind.unwrap_or(before.kind);
        let next_curate_every: Option<i64> = req
            .curate_every_secs
            .unwrap_or(before.curate_every_secs)
            .map(|v| i64::try_from(v).expect("validated curate_every_secs fits i64"));
        // `paused_at` keeps the time of the pause while the board stays paused; the hooks halt
        // only turns that began before it.
        cx.db.execute(
            "UPDATE boards SET title = ?2, goal = ?3, status = ?4, repos = ?5, autopilot = ?6, \
         home_devices = ?7, updated_at = ?8, temporary = ?9, parallel_tasks = ?10, \
         dispatch = ?11, usage_stop_at = ?12, verdict_judges = ?13, \
         goal_review_every_secs = ?14, \
         parallel_tasks_prev = ?15, parallel_tasks_until = ?16, \
         usage_stop_at_prev = ?17, usage_stop_at_until = ?18, \
         kind = ?19, curate_every_secs = ?20, \
         paused_at = CASE WHEN ?4 != 'paused' THEN NULL \
                          WHEN status = 'paused' THEN paused_at ELSE ?8 END \
         WHERE slug = ?1",
            params![
                slug.as_str(),
                req.title.unwrap_or(before.title),
                req.goal.unwrap_or(before.goal),
                enum_str(&req.status.unwrap_or(before.status)),
                serde_json::to_string(&next_repos).expect("serializes"),
                req.autopilot.unwrap_or(before.autopilot),
                serde_json::to_string(&req.home_devices.unwrap_or(before.home_devices))
                    .expect("serializes"),
                cx.now_ms(),
                req.temporary.unwrap_or(before.temporary),
                new_parallel,
                enum_str(&req.dispatch.unwrap_or(before.dispatch)),
                req.usage_stop_at.unwrap_or(before.usage_stop_at),
                req.verdict_judges.unwrap_or(before.verdict_judges),
                next_goal_every,
                next_parallel_prev,
                next_parallel_until.map(|t| t.timestamp_millis()),
                next_stop_prev,
                next_stop_until.map(|t| t.timestamp_millis()),
                enum_str(&next_kind),
                next_curate_every,
            ],
        )?;
        if new_parallel != before.parallel_tasks {
            sync_worker_write_locks(cx, slug, &next_repos, before.parallel_tasks, new_parallel)?;
        }
    }
    cx.emit(Some(slug), actor, EventType::BoardUpdated, None, payload)?;
    get_board(cx, slug)
}

/// Keeps only the task's derived `repo:<effective_repo>:write` semaphore bound on live
/// managed worker runs in sync with `board.parallel_tasks`, without touching explicit
/// cross-repo locks or widening stricter run-level locks.
fn sync_worker_write_locks(
    cx: &Ctx<'_>,
    slug: &BoardSlug,
    board_repos: &[String],
    old_parallel: u32,
    new_parallel: u32,
) -> Result<()> {
    struct WorkerWriteLock {
        key: String,
        run_id: String,
        cur_max: u32,
        task_repo: Option<String>,
        task_keys_json: Option<String>,
    }
    let rows: Vec<WorkerWriteLock> = cx
        .db
        .prepare_cached(
            "SELECT cl.key, cl.run_id, cl.max_holders, t.repo, t.conflict_keys_json \
             FROM conflict_locks cl \
             JOIN runs r ON r.id = cl.run_id \
             JOIN tasks t ON t.id = r.task_id \
             WHERE r.board = ?1 \
               AND r.kind = 'worker' \
               AND r.origin = 'managed' \
               AND cl.key LIKE 'repo:%:write'",
        )?
        .query_map([slug.as_str()], |r| {
            Ok(WorkerWriteLock {
                key: r.get(0)?,
                run_id: r.get(1)?,
                cur_max: r.get(2)?,
                task_repo: r.get(3)?,
                task_keys_json: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for WorkerWriteLock {
        key,
        run_id,
        cur_max,
        task_repo,
        task_keys_json,
    } in rows
    {
        let effective_repo = task_repo
            .as_deref()
            .or_else(|| board_repos.first().map(String::as_str));
        let Some(repo) = effective_repo else {
            continue;
        };
        if key != format!("repo:{repo}:write") {
            continue;
        }
        let explicit_min = task_keys_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<ConflictKeySpec>>(s).ok())
            .into_iter()
            .flatten()
            .filter(|spec| spec.key == key)
            .map(|spec| spec.max)
            .min();
        let prev_expected = explicit_min.map_or(old_parallel, |m| m.min(old_parallel));
        if new_parallel > old_parallel && cur_max < prev_expected {
            // Run holds a stricter lock than the board's previous derived cap; keep it.
            continue;
        }
        let derived_next = explicit_min.map_or(new_parallel, |m| m.min(new_parallel));
        let next_max = if new_parallel < old_parallel {
            cur_max.min(derived_next)
        } else {
            derived_next
        };
        if next_max != cur_max {
            cx.db.execute(
                "UPDATE conflict_locks SET max_holders = ?1 WHERE key = ?2 AND run_id = ?3",
                params![next_max, key, run_id],
            )?;
        }
    }
    Ok(())
}

/// Restores timed `parallel_tasks` and `usage_stop_at` overrides whose `until` time has
/// passed, recording a `board_updated` event as `server` for each affected board.
pub(crate) fn sweep_expired_limits(cx: &mut Ctx<'_>) -> Result<usize> {
    let now_ms = cx.now_ms();
    let slugs: Vec<String> = cx
        .db
        .prepare_cached(
            "SELECT slug FROM boards \
             WHERE (parallel_tasks_until IS NOT NULL AND parallel_tasks_until <= ?1) \
                OR (usage_stop_at_until IS NOT NULL AND usage_stop_at_until <= ?1) \
             ORDER BY slug",
        )?
        .query_map([now_ms], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut swept = 0;
    for slug_str in slugs {
        let slug: BoardSlug = parse_id(&slug_str)?;
        let before = get_board_uncounted(cx, &slug)?;
        let mut fields = Vec::new();
        let mut restored = serde_json::Map::new();
        let mut new_parallel = before.parallel_tasks;
        let mut new_parallel_prev = before.parallel_tasks_prev;
        let mut new_parallel_until = before.parallel_tasks_until;
        let mut new_stop = before.usage_stop_at;
        let mut new_stop_prev = before.usage_stop_at_prev;
        let mut new_stop_until = before.usage_stop_at_until;

        if let Some(until) = before.parallel_tasks_until
            && until.timestamp_millis() <= now_ms
        {
            let target = before.parallel_tasks_prev.unwrap_or(1);
            if target != before.parallel_tasks {
                fields.push("parallel_tasks");
            }
            fields.push("parallel_tasks_until");
            restored.insert(
                "parallel_tasks".into(),
                serde_json::json!({
                    "from": before.parallel_tasks,
                    "to": target,
                    "until": until,
                }),
            );
            new_parallel = target;
            new_parallel_prev = None;
            new_parallel_until = None;
        }
        if let Some(until) = before.usage_stop_at_until
            && until.timestamp_millis() <= now_ms
        {
            let target = before
                .usage_stop_at_prev
                .unwrap_or(board_proto::USAGE_STOP_AT_DEFAULT);
            if target != before.usage_stop_at {
                fields.push("usage_stop_at");
            }
            fields.push("usage_stop_at_until");
            restored.insert(
                "usage_stop_at".into(),
                serde_json::json!({
                    "from": before.usage_stop_at,
                    "to": target,
                    "until": until,
                }),
            );
            new_stop = target;
            new_stop_prev = None;
            new_stop_until = None;
        }
        if restored.is_empty() {
            continue;
        }
        cx.db.execute(
            "UPDATE boards SET parallel_tasks = ?2, parallel_tasks_prev = ?3, \
             parallel_tasks_until = ?4, usage_stop_at = ?5, usage_stop_at_prev = ?6, \
             usage_stop_at_until = ?7, updated_at = ?8 WHERE slug = ?1",
            params![
                slug.as_str(),
                new_parallel,
                new_parallel_prev,
                new_parallel_until.map(|t| t.timestamp_millis()),
                new_stop,
                new_stop_prev,
                new_stop_until.map(|t| t.timestamp_millis()),
                now_ms,
            ],
        )?;
        if new_parallel != before.parallel_tasks {
            sync_worker_write_locks(
                cx,
                &slug,
                &before.repos,
                before.parallel_tasks,
                new_parallel,
            )?;
        }
        let mut payload = serde_json::json!({
            "fields": fields,
            "expired": true,
            "restored": serde_json::Value::Object(restored),
        });
        if new_parallel != before.parallel_tasks {
            payload["parallel_tasks"] = serde_json::Value::from(new_parallel);
        }
        if new_stop != before.usage_stop_at {
            payload["usage_stop_at"] = serde_json::Value::from(new_stop);
        }
        cx.emit(
            Some(&slug),
            "server",
            EventType::BoardUpdated,
            None,
            payload,
        )?;
        swept += 1;
    }
    Ok(swept)
}

/// Deletes a closed board without open asks, with its tasks, notes, evidence, asks,
/// members and events. Records one `board_updated` event (`fields: ["deleted"]`), an
/// existing type, so clients that predate deletion still read the feed. Tasks on other
/// boards that waited on one of its tasks lose that blocker, each with a `task_updated`
/// event on its own board, so a task that becomes ready is announced. The slug keeps its
/// task numbering for a board created later under it.
pub(crate) fn delete(cx: &mut Ctx<'_>, actor: &str, slug: &BoardSlug) -> Result<()> {
    let board = get_board(cx, slug)?;
    if board.status != BoardStatus::Closed {
        return Err(bad_transition(
            format!("board {slug} is {}", enum_str(&board.status)),
            format!(
                "only closed boards may be deleted; close it first (board board update {slug} \
                 --status closed)"
            ),
        ));
    }
    if board.open_asks > 0 {
        return Err(bad_transition(
            format!("board {slug} has {} open asks", board.open_asks),
            format!("answer them first (board asks --board {slug}), then delete the board"),
        ));
    }
    const TASKS: &str = "SELECT id FROM tasks WHERE board = ?1";
    let s = slug.as_str();
    // Unfinished tasks elsewhere that wait on this board's tasks, with the blockers they
    // lose. Finished ones lose the link too, unannounced: nothing waits on it any more.
    let mut dependents: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for row in cx
        .db
        .prepare_cached(
            "SELECT t.id, t.board, b.blocker FROM task_blockers b JOIN tasks t ON t.id = b.task \
             JOIN tasks bt ON bt.id = b.blocker \
             WHERE bt.board = ?1 AND t.board != ?1 AND t.status NOT IN ('done', 'dropped') \
             ORDER BY t.id, bt.number",
        )?
        .query_map([s], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
    {
        let (task, board, blocker) = row?;
        dependents
            .entry(task)
            .or_insert_with(|| (board, Vec::new()))
            .1
            .push(blocker);
    }
    for sql in [
        format!("DELETE FROM task_blockers WHERE task IN ({TASKS}) OR blocker IN ({TASKS})"),
        format!("DELETE FROM notes WHERE task IN ({TASKS})"),
        format!("DELETE FROM evidence WHERE task IN ({TASKS})"),
        format!("DELETE FROM claim_refusals WHERE task IN ({TASKS})"),
        format!("DELETE FROM feedback WHERE task IN ({TASKS})"),
        format!("DELETE FROM feedback_reports WHERE task IN ({TASKS})"),
        // Leases belong to agents and expire on their own; only the task link goes.
        format!("UPDATE leases SET task = NULL WHERE task IN ({TASKS})"),
        // Lease events name a task but no board.
        format!("DELETE FROM events WHERE board = ?1 OR task IN ({TASKS})"),
        "DELETE FROM asks WHERE board = ?1".to_owned(),
        "DELETE FROM said WHERE board = ?1".to_owned(),
        "DELETE FROM judges WHERE board = ?1".to_owned(),
        "DELETE FROM stood_down WHERE board = ?1".to_owned(),
        // Parents are on the same board, so no task elsewhere refers to these.
        "UPDATE tasks SET parent = NULL WHERE board = ?1".to_owned(),
        "INSERT OR REPLACE INTO deleted_boards (slug, next_number) \
         SELECT slug, next_number FROM boards WHERE slug = ?1"
            .to_owned(),
        "DELETE FROM tasks WHERE board = ?1".to_owned(),
        "DELETE FROM board_members WHERE board = ?1".to_owned(),
        "DELETE FROM boards WHERE slug = ?1".to_owned(),
    ] {
        cx.db.execute(&sql, [s])?;
    }
    cx.emit(
        Some(slug),
        actor,
        EventType::BoardUpdated,
        None,
        serde_json::json!({ "fields": ["deleted"], "title": board.title }),
    )?;
    let now = cx.now_ms();
    for (task, (board, removed)) in dependents {
        cx.db.execute(
            "UPDATE tasks SET version = version + 1, updated_at = ?2 WHERE id = ?1",
            params![task, now],
        )?;
        let (task, board): (TaskId, BoardSlug) = (parse_id(&task)?, parse_id(&board)?);
        cx.emit(
            Some(&board),
            actor,
            EventType::TaskUpdated,
            Some(&task),
            serde_json::json!({
                "fields": ["blockers"],
                "blockers_removed": removed,
                "board_deleted": slug,
            }),
        )?;
    }
    Ok(())
}

/// Resumes the feedback board when it is paused, a state left from servers that allowed it,
/// as the server, so the daemon dispatches it again. Returns whether it did.
pub(crate) fn resume_feedback_board(cx: &mut Ctx<'_>) -> Result<bool> {
    let slug = cx.s.feedback_board.clone();
    if !cx.exists(
        "SELECT 1 FROM boards WHERE slug = ?1 AND status = 'paused'",
        [slug.as_str()],
    )? {
        return Ok(false);
    }
    let resume = UpdateBoard {
        status: Some(BoardStatus::Active),
        ..UpdateBoard::default()
    };
    update(cx, "server", &slug, resume, false)?;
    Ok(true)
}

/// What the sweep did to one temporary board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Swept {
    Closed,
    Deleted,
}

/// Idle temporary boards: not paused (a pause waits on the user, however long it lasts), no
/// open ask, no task in `doing` with a live owner, and no task not done that an unfinished
/// task on another board waits on (deleting the board would let that task start without its
/// prerequisite). Each comes
/// with whether it is closed and how long it has been idle: the time since the newer of its
/// own change and its newest event, less the time the server was down since then. The
/// server's notices (a `task_updated` of its own that changes no field: a quiet review, an
/// owner away, a passed `not_before`) do not count: they repeat, or come, while the board
/// is left alone.
fn idle_temporary(cx: &Ctx<'_>, only: Option<&BoardSlug>) -> Result<Vec<(BoardSlug, bool, i64)>> {
    let rows: Vec<(String, String, i64)> = cx
        .db
        .prepare_cached(&format!(
            "SELECT b.slug, b.status, MAX(b.updated_at, COALESCE( \
                 (SELECT e.at FROM events e WHERE e.board = b.slug \
                  AND NOT (e.actor = 'server' AND e.type = 'task_updated' \
                           AND json_extract(e.payload, '$.fields') = '[]') \
                  ORDER BY e.seq DESC LIMIT 1), \
                 0)) \
             FROM boards b \
             WHERE b.temporary = 1 AND b.status != 'paused' \
               AND (:only IS NULL OR b.slug = :only) \
               AND NOT EXISTS (SELECT 1 FROM asks a WHERE a.board = b.slug AND a.status = 'open') \
               AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.board = b.slug \
                               AND t.status = 'doing' AND {live}) \
               AND NOT EXISTS (SELECT 1 FROM task_blockers tb \
                               JOIN tasks bt ON bt.id = tb.blocker \
                               JOIN tasks wt ON wt.id = tb.task \
                               WHERE bt.board = b.slug AND bt.status != 'done' \
                               AND wt.board != b.slug \
                               AND wt.status NOT IN ('done', 'dropped')) \
             ORDER BY b.slug",
            live = live_sql("t.owner")
        ))?
        .query_map(
            rusqlite::named_params! {
                ":now": cx.now_ms(),
                ":cutoff": cx.stale_cutoff(),
                ":only": only.map(BoardSlug::as_str),
            },
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(slug, status, last)| {
            Ok((
                parse_id(&slug)?,
                status == enum_str(&BoardStatus::Closed),
                cx.now_ms() - last - downtime_since(cx, last)?,
            ))
        })
        .collect()
}

/// Records that the server run started at `started` is alive now. The sweeper calls it
/// every minute, before it sweeps temporary boards, and at shutdown, so the next run knows
/// how long the server was down.
pub(crate) fn mark_alive(cx: &mut Ctx<'_>, started: i64) -> Result<()> {
    cx.db
        .prepare_cached(
            "INSERT INTO server_runs (started_at, alive_at) VALUES (?1, ?2) \
             ON CONFLICT (started_at) DO UPDATE SET alive_at = excluded.alive_at",
        )?
        .execute(params![started, cx.now_ms()])?;
    Ok(())
}

/// How long the server was down after `last`: of each run that started after it, the time
/// from the later of `last` and the run before's last sign of life to its start.
fn downtime_since(cx: &Ctx<'_>, last: i64) -> Result<i64> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT COALESCE(SUM(MAX(0, started_at - MAX(before_alive, ?1))), 0) FROM ( \
               SELECT started_at, LAG(alive_at) OVER (ORDER BY started_at) AS before_alive \
               FROM server_runs) \
             WHERE before_alive IS NOT NULL AND started_at > ?1",
        )?
        .query_row([last], |r| r.get(0))?)
}

/// What is due for a board idle for `idle`: closing once idle for
/// `temporary_close_after_secs`, deleting once closed and idle for
/// `temporary_delete_after_secs`.
fn due(cx: &Ctx<'_>, closed: bool, idle: i64) -> Option<Swept> {
    if closed {
        (idle >= cx.s.temporary_delete_ms()).then_some(Swept::Deleted)
    } else {
        (idle >= cx.s.temporary_close_ms()).then_some(Swept::Closed)
    }
}

/// Temporary boards due to be closed or deleted.
pub(crate) fn temporary_due(cx: &Ctx<'_>) -> Result<Vec<BoardSlug>> {
    Ok(idle_temporary(cx, None)?
        .into_iter()
        .filter(|&(_, closed, idle)| due(cx, closed, idle).is_some())
        .map(|(slug, _, _)| slug)
        .collect())
}

/// Closes or deletes one temporary board if that is still due. The sweep runs one
/// transaction per board, so a failure on one does not hold up the others.
pub(crate) fn sweep_temporary(cx: &mut Ctx<'_>, slug: &BoardSlug) -> Result<Option<Swept>> {
    let Some((_, closed, idle)) = idle_temporary(cx, Some(slug))?.pop() else {
        return Ok(None);
    };
    let swept = due(cx, closed, idle);
    match swept {
        Some(Swept::Deleted) => delete(cx, "server", slug)?,
        Some(Swept::Closed) => {
            let close = UpdateBoard {
                status: Some(BoardStatus::Closed),
                ..UpdateBoard::default()
            };
            update(cx, "server", slug, close, false)?;
        }
        None => {}
    }
    Ok(swept)
}

pub(crate) fn join(cx: &mut Ctx<'_>, slug: &BoardSlug, agent: &AgentId) -> Result<Board> {
    get_board_uncounted(cx, slug)?;
    touch_agent(cx, agent)?;
    join_board(cx, slug, agent)?;
    get_board(cx, slug)
}

struct Lease {
    agent: Option<String>,
    device: Option<String>,
    expires_at: Option<i64>,
    autopilot: bool,
    status: String,
}

fn coordinator_lease(cx: &Ctx<'_>, slug: &BoardSlug) -> Result<Lease> {
    cx.db
        .query_row(
            "SELECT coordinator, coordinator_device, coordinator_expires_at, autopilot, status \
             FROM boards WHERE slug = ?1",
            [slug.as_str()],
            |r| {
                Ok(Lease {
                    agent: r.get(0)?,
                    device: r.get(1)?,
                    expires_at: r.get(2)?,
                    autopilot: r.get(3)?,
                    status: r.get(4)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| board_missing(slug))
}

/// Takes or renews the coordinator lease for `req.device`; `req.agent`, once the thread
/// exists, names the coordinator.
pub(crate) fn claim_coordinator(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    req: ClaimCoordinator,
) -> Result<Board> {
    let lease = coordinator_lease(cx, slug)?;
    if !lease.autopilot {
        return Err(invalid(
            format!("board {slug} is not on autopilot"),
            format!("switch autopilot on (PATCH /v1/boards/{slug}) before starting a coordinator"),
        ));
    }
    if lease.status != enum_str(&BoardStatus::Active) {
        return Err(invalid(
            format!("board {slug} is {}", lease.status),
            "only active boards run a coordinator; set the board active first",
        ));
    }
    let now = cx.now_ms();
    let held = lease.expires_at.is_some_and(|e| e > now);
    if held && lease.device.as_deref() != Some(req.device.as_str()) {
        let holder = lease.device.unwrap_or_default();
        let left = lease.expires_at.unwrap_or(now) - now;
        return Err(refused(
            format!(
                "device {holder} holds the coordinator lease of {slug} for {} more",
                ago(chrono::TimeDelta::milliseconds(left))
            ),
            "leave the coordinator to that device; retry after the lease expires",
        ));
    }
    if let Some(agent) = &req.agent
        && let Some(device) = agent.dispatch_device()
        && device != req.device
    {
        return Err(forbidden(
            format!(
                "{agent} is device {device}'s dispatch agent, not device {}'s",
                req.device
            ),
            format!(
                "name your own device's dispatch agent: agent={}",
                AgentId::dispatch(&req.device)
            ),
        ));
    }
    if let Some(agent) = &req.agent {
        // A device claims or renews the lease for the agent: no sign the agent itself is at
        // work, so its last_seen stays, and an agent left marked running is found away.
        let known: bool = cx.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM agents WHERE id = ?1)",
            [agent.as_str()],
            |r| r.get(0),
        )?;
        if !known {
            return Err(agent_missing(agent));
        }
        join_board(cx, slug, agent)?;
        cx.db.execute(
            "UPDATE agents SET role = 'coordinator' WHERE id = ?1",
            [agent.as_str()],
        )?;
    }
    // A renewal by the same device keeps the coordinator it named before.
    let agent = match (&req.agent, held) {
        (Some(agent), _) => Some(agent.to_string()),
        (None, true) => lease.agent.clone(),
        (None, false) => None,
    };
    // No claim changes who is stood down: the thread a board last ran as its coordinator
    // stays held, since no board runs one any more.
    cx.db.execute(
        "UPDATE boards SET coordinator = ?2, coordinator_device = ?3, coordinator_expires_at = ?4 \
         WHERE slug = ?1",
        params![
            slug.as_str(),
            agent,
            req.device.as_str(),
            expires(cx, cx.s.coordinator_ms())
        ],
    )?;
    if !held || agent != lease.agent {
        cx.db.execute(
            "UPDATE boards SET updated_at = ?2 WHERE slug = ?1",
            params![slug.as_str(), now],
        )?;
        cx.emit(
            Some(slug),
            actor,
            EventType::BoardUpdated,
            None,
            serde_json::json!({
                "fields": ["coordinator"],
                "coordinator": agent,
                "device": req.device,
            }),
        )?;
    }
    get_board(cx, slug)
}

/// Releases the coordinator lease. Only the holding device or the user may.
pub(crate) fn release_coordinator(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    device: Option<&DeviceName>,
    is_user: bool,
) -> Result<Board> {
    let lease = coordinator_lease(cx, slug)?;
    let held = lease.expires_at.is_some_and(|e| e > cx.now_ms());
    if !held {
        return get_board(cx, slug);
    }
    if !is_user && lease.device.as_deref() != device.map(DeviceName::as_str) {
        return Err(forbidden(
            format!(
                "device {} holds the coordinator lease of {slug}",
                lease.device.unwrap_or_default()
            ),
            "only the holding device or the user may release it",
        ));
    }
    cx.db.execute(
        "UPDATE boards SET coordinator = NULL, coordinator_device = NULL, \
         coordinator_expires_at = NULL, updated_at = ?2 WHERE slug = ?1",
        params![slug.as_str(), cx.now_ms()],
    )?;
    cx.emit(
        Some(slug),
        actor,
        EventType::BoardUpdated,
        None,
        // The device that held the lease, so its daemon can recognize its own release.
        serde_json::json!({ "fields": ["coordinator"], "coordinator": null, "device": lease.device }),
    )?;
    get_board(cx, slug)
}
