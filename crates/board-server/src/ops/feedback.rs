//! Feedback items (`docs/API.md`, "Feedback"): reports about any setup, deduplicated by
//! signature onto the newest live item, resolved by routing them to work, and filed by the
//! server itself when agents keep hitting the same API error.

use board_proto::{
    AgentId, Board, CLIENT_ERROR_MESSAGE_CHARS_MAX, CLIENT_ERROR_ROUTES, ClientError, CreateBoard,
    CreateFeedback, ERRORS_LIMIT_DEFAULT, ERRORS_LIMIT_MAX, ERRORS_Q_CHARS_MAX, ErrorCode,
    ErrorLog, ErrorSignature, EventType, EvidenceKind, FEEDBACK_ABOUT_MAX, FEEDBACK_SIGNATURE_MAX,
    Feedback, FeedbackActivity, FeedbackResolution, FeedbackResult, FeedbackSource, FeedbackStats,
    NewEvidence, ResolutionKind, ResolveFeedback, ResolvedCounts, Task, TaskId, Timestamp,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Settings;
use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ts};
use crate::error::{Result, bad_transition, forbidden, invalid};
use crate::ops::tasks::status_str;
use crate::ops::{boards, touch_agent};
use crate::store::{find_board, find_task, get_task};
use crate::text::{check_line, check_text_or, cut};

/// Agents an item lists in `reporters`: the most recent ones.
const REPORTERS_MAX: usize = 20;
/// A repeat report adds a note at most this often per item.
const NOTE_EVERY_MS: i64 = 3_600_000;
/// Duplicate links followed at most, in case they form a loop.
const DUPLICATE_DEPTH_MAX: usize = 8;
/// Characters of a normalized error message kept.
const TEMPLATE_MAX: usize = 200;

/// A validated report.
#[derive(Debug, Clone)]
pub(crate) struct Report {
    pub about: String,
    pub title: String,
    pub body: String,
    pub activity: Option<FeedbackActivity>,
    pub source: FeedbackSource,
    pub signature: String,
    pub reporter: AgentId,
}

/// Checks a report from `reporter` and works out its title and signature.
pub(crate) fn validate_create(
    req: CreateFeedback,
    s: &Settings,
    reporter: AgentId,
) -> Result<Report> {
    let about = req.about.trim().to_lowercase();
    let about_ok = (1..=FEEDBACK_ABOUT_MAX).contains(&about.chars().count())
        && about
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '_' | '-'));
    if !about_ok {
        return Err(invalid(
            format!("about {:?} is not a setup name", req.about),
            format!(
                "send 1-{FEEDBACK_ABOUT_MAX} characters of a-z 0-9 : _ - naming the setup, such \
                 as board, skills:rust or t3; use other when unsure"
            ),
        ));
    }
    let source = req.source.unwrap_or_default();
    if source == FeedbackSource::Signal {
        return Err(invalid(
            "source signal is the server's own",
            "send source agent (the default) or capture",
        ));
    }
    let text = req.text.trim();
    if text.is_empty() {
        return Err(invalid(
            "feedback text is empty",
            "say what cost you a step: the first line is the title",
        ));
    }
    check_text_or(
        "text",
        text,
        s.body_chars,
        Some(board_proto::FEEDBACK_LONGER_TEXT_GOES),
    )?;
    let first: String = text
        .split(['\n', '\r'])
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let title = cut(&first, s.title_chars);
    let signature = match req.signature.as_deref().map(str::trim) {
        Some(sig) => {
            check_line("signature", sig, FEEDBACK_SIGNATURE_MAX)?;
            sig.to_owned()
        }
        None => derived_signature(&about, &title),
    };
    Ok(Report {
        about,
        title,
        body: text.to_owned(),
        activity: req.activity,
        source,
        signature,
        reporter,
    })
}

/// The signature of a report that names none: `about` and the title, lowercased, with
/// ids and numbers normalized.
pub(crate) fn derived_signature(about: &str, title: &str) -> String {
    format!("{about}:{}", normalize(title, false))
        .chars()
        .take(FEEDBACK_SIGNATURE_MAX)
        .collect()
}

/// Lowercases `text` and replaces agent ids with `AGENT`, task ids with `ID` and digit
/// runs with `N`; with `quotes`, text in double quotes or backticks becomes `"…"`.
pub(crate) fn normalize(text: &str, quotes: bool) -> String {
    fn word_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '%' | '.')
    }
    fn flush(word: &mut String, out: &mut String) {
        let core = word.trim_end_matches(['.', ':', '-', '_']);
        let tail = &word[core.len()..];
        if core.parse::<AgentId>().is_ok() {
            out.push_str("AGENT");
        } else if core.chars().any(|c| c.is_alphabetic()) && core.parse::<TaskId>().is_ok() {
            out.push_str("ID");
        } else {
            let mut digits = false;
            for c in core.chars() {
                if c.is_ascii_digit() {
                    if !digits {
                        out.push('N');
                    }
                    digits = true;
                } else {
                    out.push(c);
                    digits = false;
                }
            }
        }
        out.push_str(tail);
        word.clear();
    }
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut word = String::new();
    let mut chars = lower.chars();
    while let Some(c) = chars.next() {
        if quotes && matches!(c, '"' | '`') {
            flush(&mut word, &mut out);
            let mut escaped = false;
            for q in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if q == '\\' && c == '"' {
                    escaped = true;
                } else if q == c {
                    break;
                }
            }
            out.push_str("\"…\"");
        } else if word_char(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Files a report: adds it to the live item with its signature, or files a new item.
pub(crate) fn report(cx: &mut Ctx<'_>, r: Report) -> Result<FeedbackResult> {
    touch_agent(cx, &r.reporter)?;
    let actor = r.reporter.to_string();
    let (task, duplicate) = match prior(cx, &r.signature)? {
        Prior::Live(item) => (add_report(cx, &item, &r.reporter, &actor, &r.title)?, true),
        Prior::Gone(earlier) => (
            file(cx, &r, &actor, vec![r.reporter.clone()], 1, Some(earlier))?,
            false,
        ),
        Prior::None => (
            file(cx, &r, &actor, vec![r.reporter.clone()], 1, None)?,
            false,
        ),
    };
    if !duplicate {
        record_report(cx, &task.id, &r.reporter)?;
    }
    let reports = task.feedback.as_ref().map_or(1, |f| f.reports);
    Ok(FeedbackResult {
        task,
        duplicate,
        reports,
    })
}

/// The newest item with a signature: live, or no longer live.
enum Prior {
    /// Reports go to this item (an original, when the newest one was a duplicate).
    Live(TaskId),
    /// The newest item, no longer live: a new item recurs it.
    Gone(TaskId),
    None,
}

fn prior(cx: &Ctx<'_>, signature: &str) -> Result<Prior> {
    let newest: Option<String> = cx
        .db
        .query_row(
            "SELECT task FROM feedback WHERE signature = ?1 ORDER BY rowid DESC LIMIT 1",
            [signature],
            |r| r.get(0),
        )
        .optional()?;
    let Some(newest) = newest else {
        return Ok(Prior::None);
    };
    let newest: TaskId = parse_id(&newest)?;
    Ok(match live_item(cx, &newest, 0)? {
        Some(item) => Prior::Live(item),
        None => Prior::Gone(newest),
    })
}

/// The item that takes reports for feedback item `id`, if it is live: unresolved and not
/// done or dropped, declined, a duplicate of a live item, or routed to a task that is not
/// done or dropped. Forwarded and fixed items are not live.
fn live_item(cx: &Ctx<'_>, id: &TaskId, depth: usize) -> Result<Option<TaskId>> {
    let row: Option<(Option<String>, Option<String>)> = cx
        .db
        .query_row(
            "SELECT resolution, resolution_task FROM feedback WHERE task = ?1",
            [id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((kind, target)) = row else {
        return Ok(None);
    };
    let kind: Option<ResolutionKind> = kind.map(|k| parse_enum(&k)).transpose()?;
    let target: Option<TaskId> = target.map(|t| parse_id(&t)).transpose()?;
    Ok(match (kind, target) {
        (None, _) => task_open(cx, id)?.then(|| id.clone()),
        (Some(ResolutionKind::Declined), _) => Some(id.clone()),
        (Some(ResolutionKind::Duplicate), Some(original))
            if depth < DUPLICATE_DEPTH_MAX && is_feedback(cx, &original)? =>
        {
            live_item(cx, &original, depth + 1)?
        }
        // A duplicate of a plain task counts as routed to it.
        (Some(ResolutionKind::Routed | ResolutionKind::Duplicate), Some(task)) => {
            task_open(cx, &task)?.then(|| id.clone())
        }
        _ => None,
    })
}

fn is_feedback(cx: &Ctx<'_>, id: &TaskId) -> Result<bool> {
    cx.exists("SELECT 1 FROM feedback WHERE task = ?1", [id.as_str()])
}

/// The task exists and is neither done nor dropped.
fn task_open(cx: &Ctx<'_>, id: &TaskId) -> Result<bool> {
    Ok(task_status(cx, id)?.is_some_and(|s| s != "done" && s != "dropped"))
}

fn task_status(cx: &Ctx<'_>, id: &TaskId) -> Result<Option<String>> {
    Ok(cx
        .db
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()?)
}

/// The feedback board, created on first use.
fn feedback_board(cx: &mut Ctx<'_>, actor: &str) -> Result<Board> {
    let slug = cx.s.feedback_board.clone();
    if let Some(board) = find_board(cx, &slug)? {
        return Ok(board);
    }
    boards::create(
        cx,
        actor,
        CreateBoard {
            slug,
            title: "Feedback".to_owned(),
            goal: "Reports about any setup (the board, instructions, skills, hooks, tools, \
                   storage, a project's setup) that cost an agent a step, and the server's \
                   own signals; each is routed to work or resolved."
                .to_owned(),
            repos: Vec::new(),
            autopilot: false,
            home_devices: Vec::new(),
            temporary: false,
        },
    )
}

/// Files a new item for `r` on the feedback board.
fn file(
    cx: &mut Ctx<'_>,
    r: &Report,
    actor: &str,
    reporters: Vec<AgentId>,
    reports: u32,
    recurs: Option<TaskId>,
) -> Result<Task> {
    let board = feedback_board(cx, actor)?;
    let number: i64 = cx.db.query_row(
        "UPDATE boards SET next_number = next_number + 1 WHERE slug = ?1 RETURNING next_number - 1",
        [board.slug.as_str()],
        |row| row.get(0),
    )?;
    let id = TaskId::new(&board.slug, number as u64);
    let now = cx.now_ms();
    cx.db.execute(
        "INSERT INTO tasks (id, board, number, title, body, acceptance, status, priority, auto, \
         created_by, version, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, '', 'open', 'p2', ?6, ?7, 1, ?8, ?8)",
        params![
            id.as_str(),
            board.slug.as_str(),
            number,
            r.title,
            r.body,
            board.autopilot,
            r.reporter.as_str(),
            now,
        ],
    )?;
    cx.db.execute(
        "INSERT INTO feedback (task, about, activity, source, signature, reports, reporters, \
         last_reported_at, recurs) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id.as_str(),
            r.about,
            r.activity.map(|a| enum_str(&a)),
            enum_str(&r.source),
            r.signature,
            reports,
            serde_json::to_string(&reporters).expect("ids serialize"),
            now,
            recurs.as_ref().map(TaskId::as_str),
        ],
    )?;
    cx.emit(
        Some(&board.slug),
        actor,
        EventType::TaskCreated,
        Some(&id),
        serde_json::json!({
            "title": r.title,
            "priority": "p2",
            "feedback": { "about": r.about, "source": r.source, "recurs": recurs },
        }),
    )?;
    get_task(cx, &id)
}

fn record_report(cx: &Ctx<'_>, item: &TaskId, agent: &AgentId) -> Result<()> {
    cx.db.execute(
        "INSERT INTO feedback_reports (task, agent, at) VALUES (?1, ?2, ?3)",
        params![item.as_str(), agent.as_str(), cx.now_ms()],
    )?;
    Ok(())
}

/// Adds a report to a live item: counts it, records the reporter and, at most once an
/// hour, notes it (with the routed task, if any).
fn add_report(
    cx: &mut Ctx<'_>,
    item: &TaskId,
    reporter: &AgentId,
    actor: &str,
    summary: &str,
) -> Result<Task> {
    let (reports, reporters, last_noted, kind, target): (
        u32,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = cx.db.query_row(
        "SELECT reports, reporters, last_noted_at, resolution, resolution_task FROM feedback \
         WHERE task = ?1",
        [item.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let reports = reports.saturating_add(1);
    let mut reporters: Vec<AgentId> = parse_json(&reporters)?;
    reporters.retain(|a| a != reporter);
    reporters.push(reporter.clone());
    let excess = reporters.len().saturating_sub(REPORTERS_MAX);
    reporters.drain(..excess);
    let now = cx.now_ms();
    let note = last_noted.is_none_or(|at| now - at >= NOTE_EVERY_MS);
    cx.db.execute(
        "UPDATE feedback SET reports = ?2, reporters = ?3, last_reported_at = ?4, \
         last_noted_at = CASE WHEN ?5 THEN ?4 ELSE last_noted_at END WHERE task = ?1",
        params![
            item.as_str(),
            reports,
            serde_json::to_string(&reporters).expect("ids serialize"),
            now,
            note,
        ],
    )?;
    record_report(cx, item, reporter)?;
    // The count is not a field anyone edits: it leaves the version alone, so an owner's
    // update does not conflict with reports, and is announced with the hourly note.
    let board = item.board();
    if note {
        cx.db.execute(
            "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
            params![item.as_str(), now],
        )?;
        let routed = match (kind.as_deref(), target) {
            (Some("routed"), Some(task)) => {
                let task: TaskId = parse_id(&task)?;
                let status = task_status(cx, &task)?.unwrap_or_else(|| "gone".to_owned());
                format!("; routed to {task} ({status})")
            }
            _ => String::new(),
        };
        let text = cut(
            &format!("reported again ({reports} reports){routed}: {summary}"),
            cx.s.note_chars,
        );
        cx.db.execute(
            "INSERT INTO notes (task, agent, kind, text, at) VALUES (?1, ?2, 'feedback', ?3, ?4)",
            params![item.as_str(), reporter.as_str(), text, now],
        )?;
        let note_id = cx.db.last_insert_rowid();
        cx.emit(
            Some(&board),
            actor,
            EventType::NoteAdded,
            Some(item),
            serde_json::json!({ "note": note_id, "kind": "feedback" }),
        )?;
        cx.emit(
            Some(&board),
            actor,
            EventType::TaskUpdated,
            Some(item),
            serde_json::json!({ "fields": ["feedback"], "reports": reports }),
        )?;
    }
    get_task(cx, item)
}

/// The feedback part of task `id`, if it is a feedback item.
pub(crate) fn load(cx: &Ctx<'_>, id: &TaskId) -> Result<Option<Feedback>> {
    let row = cx
        .db
        .prepare_cached(
            "SELECT about, activity, source, signature, reports, reporters, last_reported_at, \
             resolution, resolution_task, resolution_reason, resolved_at, resolved_by, recurs \
             FROM feedback WHERE task = ?1",
        )?
        .query_row([id.as_str()], |r| {
            let resolution = match r.get::<_, Option<String>>(7)? {
                Some(kind) => Some(FeedbackResolution {
                    kind: parse_enum(&kind)?,
                    task: r
                        .get::<_, Option<String>>(8)?
                        .map(|t| parse_id(&t))
                        .transpose()?,
                    reason: r.get(9)?,
                    at: ts(r.get(10)?),
                    by: r.get(11)?,
                }),
                None => None,
            };
            Ok(Feedback {
                about: r.get(0)?,
                activity: r
                    .get::<_, Option<String>>(1)?
                    .map(|a| parse_enum(&a))
                    .transpose()?,
                source: parse_enum(&r.get::<_, String>(2)?)?,
                signature: r.get(3)?,
                reports: r.get(4)?,
                reporters: parse_json(&r.get::<_, String>(5)?)?,
                last_reported_at: ts(r.get(6)?),
                resolution,
                recurs: r
                    .get::<_, Option<String>>(12)?
                    .map(|t| parse_id(&t))
                    .transpose()?,
            })
        })
        .optional()?;
    Ok(row)
}

/// Forgets the resolution of a reopened item, so it is live again.
pub(crate) fn reopen(cx: &Ctx<'_>, id: &TaskId) -> Result<()> {
    cx.db.execute(
        "UPDATE feedback SET resolution = NULL, resolution_task = NULL, \
         resolution_reason = NULL, resolved_at = NULL, resolved_by = NULL WHERE task = ?1",
        [id.as_str()],
    )?;
    Ok(())
}

pub(crate) fn validate_resolve(req: &ResolveFeedback) -> Result<()> {
    let kind = enum_str(&req.kind);
    match req.kind {
        ResolutionKind::Unknown => {
            return Err(invalid(
                "unknown resolution kind",
                "send kind routed, duplicate, forwarded, declined or fixed",
            ));
        }
        ResolutionKind::Routed | ResolutionKind::Duplicate if req.task.is_none() => {
            return Err(invalid(
                format!("{kind} needs the task"),
                format!(
                    "send task: the {} (it may be on another board)",
                    if req.kind == ResolutionKind::Routed {
                        "task that does the work"
                    } else {
                        "original item"
                    }
                ),
            ));
        }
        ResolutionKind::Routed | ResolutionKind::Duplicate => {}
        ResolutionKind::Forwarded => needs_reason(req, "where it went")?,
        ResolutionKind::Declined => needs_reason(req, "why it is not acted on")?,
        ResolutionKind::Fixed => needs_reason(req, "what changed, such as the file and line")?,
    }
    if let Some(reason) = &req.reason {
        check_line("reason", reason, board_proto::RESOLUTION_REASON_CHARS_MAX)?;
    }
    Ok(())
}

/// Checks a kind that takes a reason (`hint` says what it gives) and no task.
fn needs_reason(req: &ResolveFeedback, hint: &str) -> Result<()> {
    let kind = enum_str(&req.kind);
    if req.reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
        return Err(invalid(
            format!("{kind} needs a reason"),
            format!(
                "send reason (at most {} characters): {hint}",
                board_proto::RESOLUTION_REASON_CHARS_MAX
            ),
        ));
    }
    if req.task.is_some() {
        return Err(invalid(
            format!("{kind} takes no task"),
            "route it (kind routed) to name the task that does the work",
        ));
    }
    Ok(())
}

/// Resolves a feedback item: it moves to done with the resolution recorded and artifact
/// evidence naming it. Only its owner or the user may.
pub(crate) fn resolve(
    cx: &mut Ctx<'_>,
    id: &TaskId,
    req: ResolveFeedback,
    agent: Option<AgentId>,
    by_user: bool,
) -> Result<Task> {
    if let Some(agent) = &agent {
        touch_agent(cx, agent)?;
    }
    let task = get_task(cx, id)?;
    if task.feedback.is_none() && task.board != cx.s.feedback_board {
        return Err(invalid(
            format!("{id} is not a feedback item"),
            format!("mark it done with task_done {id} and evidence"),
        ));
    }
    if task.status.is_closed() {
        return Err(bad_transition(
            format!("{id} is {}", status_str(task.status)),
            format!("reopen it first (task_update {id} status=open) to resolve it again"),
        ));
    }
    if !by_user {
        match (&task.owner, &agent) {
            (Some(owner), Some(me)) if owner == me => {}
            (Some(owner), _) => {
                return Err(forbidden(
                    format!("only the owner {owner} or the user may resolve {id}"),
                    format!("add a note instead (task_note {id}), or ask the user"),
                ));
            }
            (None, _) => {
                return Err(forbidden(
                    format!("nobody owns {id}, so you may not resolve it"),
                    format!("claim it first (task_claim {id}), then resolve it"),
                ));
            }
        }
    }
    if let Some(target) = &req.task {
        if target == id {
            return Err(invalid(
                format!("{id} cannot be resolved to itself"),
                "name the task that does the work, or the original item",
            ));
        }
        if find_task(cx, target)?.is_none() {
            return Err(invalid(
                format!("task {target} does not exist"),
                format!(
                    "check the id; task_list board={} lists its tasks",
                    target.board()
                ),
            ));
        }
    }
    let by = agent
        .as_ref()
        .map_or_else(|| "user".to_owned(), ToString::to_string);
    let kind = enum_str(&req.kind);
    let value = match (&req.task, &req.reason) {
        (Some(task), Some(reason)) => format!("feedback {kind} to {task}: {reason}"),
        (Some(task), None) => format!("feedback {kind} to {task}"),
        (None, Some(reason)) => format!("feedback {kind}: {reason}"),
        (None, None) => format!("feedback {kind}"),
    };
    let evidence = NewEvidence {
        kind: EvidenceKind::Artifact,
        value: cut(&value, board_proto::EVIDENCE_VALUE_CHARS_MAX),
        exit_code: None,
    };
    // Evidence names an agent: the resolver, else the owner, else the reporter.
    let credited = agent
        .clone()
        .or_else(|| task.owner.clone())
        .unwrap_or_else(|| task.created_by.clone());
    let now = cx.now_ms();
    if task.feedback.is_none() {
        adopt(cx, &task)?;
    }
    cx.db.execute(
        "UPDATE feedback SET resolution = ?2, resolution_task = ?3, resolution_reason = ?4, \
         resolved_at = ?5, resolved_by = ?6 WHERE task = ?1",
        params![
            id.as_str(),
            kind,
            req.task.as_ref().map(TaskId::as_str),
            req.reason.as_deref().map(str::trim),
            now,
            by,
        ],
    )?;
    cx.db.execute(
        "INSERT INTO evidence (task, kind, value, exit_code, agent, at) \
         VALUES (?1, 'artifact', ?2, NULL, ?3, ?4)",
        params![id.as_str(), evidence.value, credited.as_str(), now],
    )?;
    cx.db.execute(
        "UPDATE tasks SET status = 'done', done_at = ?2, blocked_reason = NULL, \
         version = version + 1, updated_at = ?2 WHERE id = ?1",
        params![id.as_str(), now],
    )?;
    let doing_since: Option<i64> = cx.db.query_row(
        "SELECT doing_since FROM tasks WHERE id = ?1",
        [id.as_str()],
        |r| r.get(0),
    )?;
    cx.emit(
        Some(&task.board),
        &by,
        EventType::TaskDone,
        Some(id),
        serde_json::json!({
            "evidence": [evidence],
            "doing_ms": doing_since.map(|d| now - d),
            "resolution": { "kind": req.kind, "task": req.task, "reason": req.reason },
        }),
    )?;
    get_task(cx, id)
}

/// Makes a plain task on the feedback board (filed with `task_add`, or before feedback items
/// existed) a feedback item about `other`, reported once by its creator, so it can be
/// resolved and later reports of the same title find it.
fn adopt(cx: &Ctx<'_>, task: &Task) -> Result<()> {
    let about = "other";
    cx.db.execute(
        "INSERT INTO feedback (task, about, activity, source, signature, reports, reporters, \
         last_reported_at) VALUES (?1, ?2, NULL, ?3, ?4, 1, ?5, ?6)",
        params![
            task.id.as_str(),
            about,
            enum_str(&FeedbackSource::Agent),
            derived_signature(about, &task.title),
            serde_json::to_string(&[&task.created_by]).expect("ids serialize"),
            task.created_at.timestamp_millis(),
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Signals

/// An error answer to an agent's request.
#[derive(Debug, Clone)]
pub(crate) struct ApiErrorSeen {
    pub agent: AgentId,
    pub code: ErrorCode,
    /// Method and route template, such as `POST /v1/tasks/{id}/claim`.
    pub route: String,
    pub message: String,
    pub source: ErrorSource,
}

/// Who answered a recorded error (`api_errors.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorSource {
    /// The server, to a request it received.
    Server,
    /// The client, which refused the request before sending it (`POST /v1/errors`).
    Client,
}

/// Checks a client's record of a refusal it made before sending (`POST /v1/errors`): an
/// `invalid` answer to an API route, whose message is the server's own for text over a
/// limit, so a client cannot put text of its choosing in a signal item. It is recorded for
/// `agent`, the caller, whatever the body says.
pub(crate) fn validate_client_error(req: ClientError, agent: AgentId) -> Result<ApiErrorSeen> {
    if req.code != ErrorCode::Invalid {
        return Err(invalid(
            format!("code {} is not recorded from clients", enum_str(&req.code)),
            "send code invalid: clients report only the requests they refused as invalid",
        ));
    }
    if !CLIENT_ERROR_ROUTES.contains(&req.route.as_str()) {
        return Err(invalid(
            format!("route {:?} is not an API route", cut(&req.route, 80)),
            "send the method and route template, such as POST /v1/tasks/{id}/notes \
             (docs/API.md)",
        ));
    }
    check_line("message", &req.message, CLIENT_ERROR_MESSAGE_CHARS_MAX)?;
    if board_proto::TooLong::parse(&req.message).is_none() {
        return Err(invalid(
            "message is not a refusal of text over a limit",
            "send the server's message for it, such as text is 501 characters; the limit \
             is 500, for a field clients check (docs/API.md)",
        ));
    }
    Ok(ApiErrorSeen {
        agent,
        code: req.code,
        route: req.route,
        message: req.message,
        source: ErrorSource::Client,
    })
}

/// Errors that are an expected answer, not friction, by code, route and the start of the
/// message: clients probe for an agent or a handoff that may not exist, agents race for a
/// task (refusals are counted in `claim_refusals` for the stats; other refusals of a claim,
/// such as the doing limit, still count), and the daemon renews a coordinator lease another
/// device may hold.
const EXPECTED_ERRORS: &[(ErrorCode, &str, &str)] = &[
    (ErrorCode::NotFound, "GET /v1/agents/{id}", ""),
    (ErrorCode::NotFound, "GET /v1/agents/{id}/handoff", ""),
    (
        ErrorCode::ClaimRefused,
        "POST /v1/tasks/{id}/claim",
        "claim refused:",
    ),
    (
        ErrorCode::ClaimRefused,
        "POST /v1/boards/{slug}/coordinator",
        "",
    ),
];

/// Records an error answered to an agent. Once its signature reaches the signal threshold
/// it files feedback (source `signal`); later ones add to that item while it is live.
pub(crate) fn record_error(cx: &mut Ctx<'_>, e: ApiErrorSeen) -> Result<Option<TaskId>> {
    let expected = EXPECTED_ERRORS.iter().any(|(code, route, start)| {
        *code == e.code && *route == e.route && e.message.starts_with(start)
    });
    if e.code == ErrorCode::VersionConflict || expected {
        return Ok(None);
    }
    let code = enum_str(&e.code);
    let template = cut(&normalize(&e.message, true), TEMPLATE_MAX);
    let hash = crate::auth::hex(&Sha256::digest(format!("{} {template}", e.route)));
    let signature = format!("signal:{code}:{}", &hash[..12]);
    let now = cx.now_ms();
    cx.db.execute(
        "INSERT INTO api_errors (signature, agent, code, route, template, at, source) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            signature,
            e.agent.as_str(),
            code,
            e.route,
            template,
            now,
            enum_str(&e.source)
        ],
    )?;
    let earlier = match prior(cx, &signature)? {
        Prior::Live(item) => {
            let summary = match e.source {
                ErrorSource::Server => format!("{code} from {}", e.route),
                ErrorSource::Client => {
                    format!("{code} from {}, refused by the client", e.route)
                }
            };
            add_report(cx, &item, &e.agent, "server", &summary)?;
            return Ok(Some(item));
        }
        Prior::Gone(earlier) => Some(earlier),
        Prior::None => None,
    };
    // Records outlive the signal window (error_retention_days), so only those within it
    // count. Errors that an earlier item took (before its fix) do not count towards a new one.
    let mut from = now - cx.s.signal_window_ms();
    if let Some(earlier) = &earlier {
        let last: i64 = cx.db.query_row(
            "SELECT last_reported_at FROM feedback WHERE task = ?1",
            [earlier.as_str()],
            |r| r.get(0),
        )?;
        from = from.max(last + 1);
    }
    let (count, refused): (u32, u32) = cx.db.query_row(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE source = ?3) FROM api_errors \
         WHERE signature = ?1 AND at >= ?2",
        params![signature, from, enum_str(&ErrorSource::Client)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let agents: Vec<AgentId> = cx
        .db
        .prepare_cached(
            "SELECT agent FROM api_errors WHERE signature = ?1 AND at >= ?2 \
             GROUP BY agent ORDER BY MAX(id)",
        )?
        .query_map(params![signature, from], |r| {
            parse_id(&r.get::<_, String>(0)?)
        })?
        .collect::<rusqlite::Result<_>>()?;
    if count < cx.s.signal_min_reports || (agents.len() as u32) < cx.s.signal_min_agents {
        return Ok(None);
    }
    let answered = match refused {
        0 => format!("The server answered `{code}` to {count} requests"),
        n if n == count => {
            format!("Clients refused {count} requests as `{code}` before sending them")
        }
        n => format!(
            "Agents got `{code}` on {count} requests ({} answered by the server, {n} refused \
             by the client before sending)",
            count - n
        ),
    };
    let days = cx.s.signal_window_days;
    let listed: Vec<&str> = agents.iter().map(AgentId::as_str).collect();
    let title = cut(
        &format!("{code} from {}: {template}", e.route),
        cx.s.title_chars,
    );
    let body = format!(
        "{title}\n\n{answered} from {} agents within {days} days.\n\nRoute: {}\n\
         Message (normalized): {template}\nAgents: {}",
        agents.len(),
        e.route,
        listed.join(", "),
    );
    let report = Report {
        about: "board".to_owned(),
        title,
        body: cut(&body, cx.s.body_chars),
        activity: None,
        source: FeedbackSource::Signal,
        signature,
        reporter: e.agent,
    };
    let excess = agents.len().saturating_sub(REPORTERS_MAX);
    let reporters = agents[excess..].to_vec();
    let task = file(cx, &report, "server", reporters, count, earlier)?;
    cx.db.execute(
        "INSERT INTO feedback_reports (task, agent, at) SELECT ?1, agent, at FROM api_errors \
         WHERE signature = ?2 AND at >= ?3 ORDER BY id",
        params![task.id.as_str(), report.signature, from],
    )?;
    Ok(Some(task.id))
}

/// Forgets error records older than `error_retention_days`. Run by the sweeper.
pub(crate) fn prune_signals(cx: &Ctx<'_>) -> Result<usize> {
    Ok(cx.db.execute(
        "DELETE FROM api_errors WHERE at < ?1",
        [cx.now_ms() - cx.s.error_retention_ms()],
    )?)
}

/// `GET /v1/errors` query, validated by [`ErrorsFilter::new`].
#[derive(Deserialize)]
pub(crate) struct ErrorsQuery {
    since: Option<Timestamp>,
    code: Option<ErrorCode>,
    route: Option<String>,
    q: Option<String>,
    limit: Option<u32>,
}

/// Filters of `GET /v1/errors`.
pub(crate) struct ErrorsFilter {
    since: i64,
    code: Option<ErrorCode>,
    route: Option<String>,
    /// ASCII-lowercased, as SQLite's `lower` folds the template.
    q: Option<String>,
    limit: u32,
}

impl ErrorsFilter {
    /// `since` defaults to the signal window back from now, and never reaches back past the
    /// records kept; `limit` defaults to [`ERRORS_LIMIT_DEFAULT`]. Blank text filters are
    /// left out.
    pub(crate) fn new(cx: &Ctx<'_>, q: ErrorsQuery) -> Result<Self> {
        let limit = q.limit.unwrap_or(ERRORS_LIMIT_DEFAULT);
        if !(1..=ERRORS_LIMIT_MAX).contains(&limit) {
            return Err(invalid(
                format!("limit={limit} is out of range"),
                format!("send limit between 1 and {ERRORS_LIMIT_MAX}"),
            ));
        }
        let blank = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        let text = blank(q.q);
        if let Some(text) = &text {
            check_line("q", text, ERRORS_Q_CHARS_MAX)?;
        }
        let now = cx.now_ms();
        let kept = now - cx.s.error_retention_ms();
        let since = q
            .since
            .map_or_else(|| now - cx.s.signal_window_ms(), |at| at.timestamp_millis());
        Ok(Self {
            since: since.max(kept),
            code: q.code,
            route: blank(q.route),
            q: text.map(|t| t.to_ascii_lowercase()),
            limit,
        })
    }
}

/// `GET /v1/errors`: the recorded API errors since a time, one row per signature, most
/// first, with the newest feedback item each one filed or added to.
pub(crate) fn errors(cx: &Ctx<'_>, f: &ErrorsFilter) -> Result<ErrorLog> {
    let code = f.code.as_ref().map(enum_str);
    // The item lookup runs on the rows kept, not on every signature.
    let rows = cx
        .db
        .prepare_cached(
            "SELECT g.*, (SELECT task FROM feedback WHERE signature = g.signature \
             ORDER BY rowid DESC LIMIT 1) FROM ( \
             SELECT signature, code, route, template, COUNT(*) AS n, COUNT(DISTINCT agent), \
             MIN(at), MAX(at) AS last FROM api_errors WHERE at >= ?1 \
             AND (?2 IS NULL OR code = ?2) AND (?3 IS NULL OR route = ?3) \
             AND (?4 IS NULL OR instr(lower(template), ?4) > 0) \
             GROUP BY signature ORDER BY n DESC, last DESC LIMIT ?5) g \
             ORDER BY g.n DESC, g.last DESC",
        )?
        .query_map(params![f.since, code, f.route, f.q, f.limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, u32>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let signatures = rows
        .into_iter()
        .map(
            |(signature, code, route, template, count, agents, first, last, item)| {
                Ok(ErrorSignature {
                    signature,
                    code,
                    route,
                    template,
                    count,
                    agents,
                    first_at: ts(first),
                    last_at: ts(last),
                    item: item.as_deref().map(parse_id).transpose()?,
                })
            },
        )
        .collect::<Result<_>>()?;
    Ok(ErrorLog {
        since: ts(f.since),
        retention_days: cx.s.error_retention(),
        signatures,
    })
}

/// Feedback counts since `from` (milliseconds).
pub(crate) fn stats(cx: &Ctx<'_>, from: i64) -> Result<FeedbackStats> {
    let count = |sql: &str| -> Result<u32> { Ok(cx.db.query_row(sql, [from], |r| r.get(0))?) };
    let mut resolved = ResolvedCounts::default();
    let rows: Vec<(String, u32)> = cx
        .db
        .prepare_cached(
            "SELECT resolution, COUNT(*) FROM feedback WHERE resolution IS NOT NULL \
             AND resolved_at >= ?1 GROUP BY resolution",
        )?
        .query_map([from], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (kind, n) in rows {
        match parse_enum::<ResolutionKind>(&kind)? {
            ResolutionKind::Routed => resolved.routed = n,
            ResolutionKind::Duplicate => resolved.duplicate = n,
            ResolutionKind::Forwarded => resolved.forwarded = n,
            ResolutionKind::Declined => resolved.declined = n,
            ResolutionKind::Fixed => resolved.fixed = n,
            // The server never stores it.
            ResolutionKind::Unknown => {}
        }
    }
    Ok(FeedbackStats {
        reported: count("SELECT COUNT(*) FROM feedback_reports WHERE at >= ?1")?,
        filed: count(
            "SELECT COUNT(*) FROM feedback f JOIN tasks t ON t.id = f.task \
             WHERE t.created_at >= ?1",
        )?,
        resolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_hides_ids_numbers_and_quotes() {
        assert_eq!(
            normalize(
                "claim refused: t3:c2d0 owns factory-12 (seen 2m ago); limit \"x y\" is `z`",
                true
            ),
            "claim refused: AGENT owns ID (seen Nm ago); limit \"…\" is \"…\""
        );
        assert_eq!(
            normalize("Board-Feedback-3 failed: \"keep\"", false),
            "ID failed: \"keep\""
        );
        assert_eq!(normalize(r#"bad "a \" b" end"#, true), r#"bad "…" end"#);
        assert_eq!(
            derived_signature("board", "Claims take 30s"),
            "board:claims take Ns"
        );
        // A date is numbers whatever its digits, never a task id.
        for date in ["2026-11-17", "2026-11-07"] {
            assert_eq!(
                normalize(&format!("waits until {date}"), true),
                "waits until N-N-N"
            );
        }
    }
}
