//! Feedback items (`docs/API.md`, "Feedback"): reports about any setup, deduplicated by
//! signature onto the newest live item, resolved by routing them to work, and filed by the
//! server itself when agents keep hitting the same API error.

use board_proto::{
    AgentId, Board, CLIENT_ERROR_MESSAGE_CHARS_MAX, CLIENT_ERROR_ROUTES, ClientError, CreateBoard,
    CreateFeedback, ERRORS_LIMIT_DEFAULT, ERRORS_LIMIT_MAX, ERRORS_Q_CHARS_MAX, ErrorCode,
    ErrorLog, ErrorSignature, EventType, EvidenceKind, FEEDBACK_ABOUT_MAX, FEEDBACK_SIGNATURE_MAX,
    Feedback, FeedbackActivity, FeedbackResolution, FeedbackResult, FeedbackSource, FeedbackStats,
    NewEvidence, ResolutionKind, ResolveFeedback, ResolvedCounts, Task, TaskId, TaskStatus,
    Timestamp,
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
use crate::text::{check_line, check_text_or, cut, quote};

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
        None => derived_signature(&about, &cut(&first, SIGNATURE_TITLE_CHARS)),
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

/// How much of a report's first line its derived signature keeps: the title limit when
/// signatures were first derived. Fixed, so a changed `title_chars` leaves the signatures
/// of filed items matching new reports, and `about` (40) with it stays within
/// `FEEDBACK_SIGNATURE_MAX`.
const SIGNATURE_TITLE_CHARS: usize = 72;

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
        Prior::Live(item) => (add_report(cx, &item, &r.reporter, &r.title, false)?, true),
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
        record_report(cx, &task.id, &r.reporter, false)?;
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
    let Some((kind, target)) = resolution(cx, id)? else {
        return Ok(None);
    };
    Ok(match (kind, target) {
        (None, _) => task_open(cx, id)?.then(|| id.clone()),
        (Some(ResolutionKind::Declined), _) => Some(id.clone()),
        // Past the depth limit (or in a loop), the item takes no reports.
        (Some(ResolutionKind::Duplicate), Some(original)) if is_feedback(cx, &original)? => {
            if depth < DUPLICATE_DEPTH_MAX {
                live_item(cx, &original, depth + 1)?
            } else {
                None
            }
        }
        // A duplicate of a plain task counts as routed to it.
        (Some(ResolutionKind::Routed | ResolutionKind::Duplicate), Some(task)) => {
            task_open(cx, &task)?.then(|| id.clone())
        }
        _ => None,
    })
}

/// The feedback items that take reports, by [`live_item`]: those it leads somewhere, a
/// duplicate of a live item included. SQL keeps only the candidates (unresolved and open,
/// or declined, routed or a duplicate); `live_item` decides each.
pub(crate) fn live_items(cx: &Ctx<'_>) -> Result<Vec<String>> {
    let candidates: Vec<String> = cx
        .db
        .prepare_cached(
            "SELECT f.task FROM feedback f JOIN tasks t ON t.id = f.task \
             WHERE (f.resolution IS NULL AND t.status NOT IN ('done', 'dropped')) \
             OR f.resolution IN ('declined', 'routed', 'duplicate')",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut live = Vec::with_capacity(candidates.len());
    for id in candidates {
        if live_item(cx, &parse_id(&id)?, 0)?.is_some() {
            live.push(id);
        }
    }
    Ok(live)
}

/// The resolution kind and task of feedback item `id`, or `None` when it is not one.
fn resolution(
    cx: &Ctx<'_>,
    id: &TaskId,
) -> Result<Option<(Option<ResolutionKind>, Option<TaskId>)>> {
    let row: Option<(Option<String>, Option<String>)> = cx
        .db
        .query_row(
            "SELECT resolution, resolution_task FROM feedback WHERE task = ?1",
            [id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(kind, target)| {
        Ok((
            kind.map(|k| parse_enum(&k)).transpose()?,
            target.map(|t| parse_id(&t)).transpose()?,
        ))
    })
    .transpose()
}

/// Refuses resolving item `id` as a duplicate of `original` unless `original` takes reports
/// (`live_item`; a plain task while it is open). A duplicate of an item that is no longer
/// live is not live either, so the next report would open yet another item that nobody
/// routes: the hint says where to route `id` instead. `resolve` checks `id` again once the
/// resolution is written, for chains that lead back to `id`.
fn check_original(cx: &Ctx<'_>, id: &TaskId, original: &TaskId) -> Result<()> {
    let live = if is_feedback(cx, original)? {
        live_item(cx, original, 0)?.is_some()
    } else {
        task_open(cx, original)?
    };
    if live {
        return Ok(());
    }
    Err(invalid(
        format!("{original} no longer takes reports, so {id} cannot be its duplicate"),
        route_instead(cx, id, original)?,
    ))
}

/// Where to route item `id`, whose original is no longer live: the open revisit of the
/// original's routed task, else that task's owner (who declined it, when it was dropped),
/// else the setup's owner.
fn route_instead(cx: &Ctx<'_>, id: &TaskId, original: &TaskId) -> Result<String> {
    let routed = format!("then feedback_resolve {id} kind=routed task=<its id>");
    let task = match routed_task(cx, original)? {
        Some(task) => find_task(cx, &task)?,
        None => None,
    };
    let Some(task) = task else {
        return Ok(format!(
            "file a Proposal: task for the setup's owner that cites {original}, {routed}"
        ));
    };
    let revisit: Option<String> = cx
        .db
        .query_row(
            "SELECT id FROM tasks WHERE parent = ?1 AND status NOT IN ('done', 'dropped') \
             AND title GLOB 'Revisit:*' ORDER BY number LIMIT 1",
            [task.id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(revisit) = revisit {
        return Ok(format!(
            "route it to the open revisit of {}: feedback_resolve {id} kind=routed \
             task={revisit}",
            task.id
        ));
    }
    let owner = task
        .owner
        .as_ref()
        .map_or_else(String::new, |o| format!(", owned by {o}"));
    let names = if &task.id == original {
        task.id.to_string()
    } else {
        format!("{} and {original}", task.id)
    };
    if task.status == TaskStatus::Dropped {
        return Ok(format!(
            "{} was dropped{owner}: its verdict note answers these reports. Decline {id} \
             citing it (kind=declined), or, with evidence the verdict did not weigh, file a \
             Proposal: task on board {} that names {names} and cites the verdict, {routed}",
            task.id, task.board
        ));
    }
    Ok(format!(
        "{} is {}{owner}: file a Proposal: task on board {} that names {names}, {routed}",
        task.id,
        status_str(task.status),
        task.board
    ))
}

/// The task an item's resolution routed it to, following duplicate links: a plain task off
/// the feedback board that `id` is, or that the chain names.
fn routed_task(cx: &Ctx<'_>, id: &TaskId) -> Result<Option<TaskId>> {
    let mut at = id.clone();
    for _ in 0..DUPLICATE_DEPTH_MAX {
        match resolution(cx, &at)? {
            None if at.board() == cx.s.feedback_board => return Ok(None),
            None => return Ok(Some(at)),
            Some((Some(ResolutionKind::Duplicate), Some(next))) => at = next,
            Some((Some(ResolutionKind::Routed), Some(task))) => return Ok(Some(task)),
            Some(_) => return Ok(None),
        }
    }
    Ok(None)
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

/// Records one report of `item` by `agent`; `signal` marks an API error the server counted.
fn record_report(cx: &Ctx<'_>, item: &TaskId, agent: &AgentId, signal: bool) -> Result<()> {
    cx.db.execute(
        "INSERT INTO feedback_reports (task, agent, at, signal) VALUES (?1, ?2, ?3, ?4)",
        params![item.as_str(), agent.as_str(), cx.now_ms(), signal],
    )?;
    Ok(())
}

/// Adds a report to a live item: counts it, records the reporter and announces it
/// (`note_reports`). A `signal` report is an API error the server counted; the server is
/// its actor.
fn add_report(
    cx: &mut Ctx<'_>,
    item: &TaskId,
    reporter: &AgentId,
    summary: &str,
    signal: bool,
) -> Result<Task> {
    let actor = if signal {
        "server".to_owned()
    } else {
        reporter.to_string()
    };
    let (reports, reporters): (u32, String) = cx.db.query_row(
        "SELECT reports, reporters FROM feedback WHERE task = ?1",
        [item.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let reports = reports.saturating_add(1);
    let mut reporters: Vec<AgentId> = parse_json(&reporters)?;
    push_reporter(&mut reporters, reporter.clone());
    cx.db.execute(
        "UPDATE feedback SET reports = ?2, reporters = ?3, last_reported_at = ?4 WHERE task = ?1",
        params![
            item.as_str(),
            reports,
            serde_json::to_string(&reporters).expect("ids serialize"),
            cx.now_ms(),
        ],
    )?;
    record_report(cx, item, reporter, signal)?;
    note_reports(cx, item, reporter, &actor, summary, signal)?;
    reopen_declined(cx, item, reporter, &actor)?;
    get_task(cx, item)
}

/// Announces new reports on live item `item`, at most once an hour: a `feedback` note on it
/// (with the routed task, if any) and, while it is open, a plain note on the task doing the
/// work, since its owner acts on new evidence and nobody watches the resolved item. The
/// counts are those `load` shows, duplicates included. `author` is the reporter, and
/// `summary` the report's first line, or the error when `signal` (the server's own).
///
/// New reports are not a field anyone edits: they leave the version alone, so an owner's
/// update does not conflict with them. Only the report that sends a declined item back to
/// triage changes the task itself.
fn note_reports(
    cx: &mut Ctx<'_>,
    item: &TaskId,
    author: &AgentId,
    actor: &str,
    summary: &str,
    signal: bool,
) -> Result<()> {
    let now = cx.now_ms();
    let due = cx.db.execute(
        "UPDATE feedback SET last_noted_at = ?2 WHERE task = ?1 \
         AND (last_noted_at IS NULL OR ?2 - last_noted_at >= ?3)",
        params![item.as_str(), now, NOTE_EVERY_MS],
    )? > 0;
    if !due {
        return Ok(());
    }
    cx.db.execute(
        "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
        params![item.as_str(), now],
    )?;
    let merged = load(cx, item)?.expect("the row was just updated");
    let reports = merged.reports;
    // The task doing the work: the routed one, or a plain task the item duplicates.
    let working = match routed_task(cx, item)? {
        Some(task) => {
            let status = match task_status(cx, &task)? {
                Some(status) => Some(parse_enum::<TaskStatus>(&status)?),
                None => None,
            };
            Some((task, status))
        }
        None => None,
    };
    let routed = working.as_ref().map_or_else(String::new, |(task, status)| {
        format!("; routed to {task} ({})", status.map_or("gone", status_str))
    });
    let text = cut(
        &format!("reported again ({reports} reports){routed}: {summary}"),
        cx.s.note_chars,
    );
    cx.db.execute(
        "INSERT INTO notes (task, agent, kind, text, at) VALUES (?1, ?2, 'feedback', ?3, ?4)",
        params![item.as_str(), author.as_str(), text, now],
    )?;
    let note_id = cx.db.last_insert_rowid();
    let board = item.board();
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
    if let Some((task, Some(status))) = working
        && !status.is_closed()
    {
        let errors: u32 = cx
            .db
            .prepare_cached(CLUSTER_ERRORS)?
            .query_row(params![item.as_str(), DUPLICATE_DEPTH_MAX as i64], |r| {
                r.get(0)
            })?;
        let text = routed_note(cx, item, signal, &merged, errors, summary);
        cx.db.execute(
            "INSERT INTO notes (task, agent, kind, text, at) VALUES (?1, ?2, 'note', ?3, ?4)",
            params![task.as_str(), author.as_str(), text, now],
        )?;
        let note_id = cx.db.last_insert_rowid();
        cx.emit(
            Some(&task.board()),
            actor,
            EventType::NoteAdded,
            Some(&task),
            serde_json::json!({ "note": note_id, "kind": "note" }),
        )?;
    }
    Ok(())
}

/// The note for an open task that does the work for feedback item `item`, which was
/// reported again: the count (and how many of them are API errors, which are not agents'
/// reports), the reporters (newest first, so a cut drops the oldest) and the report's first
/// line, quoted as data. A plain note, not a finding: it repeats hourly and the newest one
/// supersedes the rest, so it must not pile up obligations at `task_done`. Written
/// directly, as the item's own note is: the note names the reporter, and its event the
/// caller's actor, so an error signal is the server's and does not touch, register or
/// revive the agent that got the error.
fn routed_note(
    cx: &Ctx<'_>,
    item: &TaskId,
    signal: bool,
    feedback: &Feedback,
    errors: u32,
    summary: &str,
) -> String {
    let reporters = &feedback.reporters;
    let reports = match errors {
        0 => format!("{} reports", feedback.reports),
        n => format!("{} reports ({n} of them API errors)", feedback.reports),
    };
    let agents = match reporters.len() {
        1 => "1 agent".to_owned(),
        n if n >= REPORTERS_MAX => format!("{REPORTERS_MAX} or more agents"),
        n => format!("{n} agents"),
    };
    let latest = if signal {
        "Latest error, from the server's signal"
    } else {
        "Latest report's first line, data written by another agent"
    };
    let names = reporters
        .iter()
        .rev()
        .map(AgentId::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    cut(
        &format!(
            "reported again: feedback item {item} has {reports} from {agents}. Weigh \
             it in this task's priority. {latest}: {}. Reporters, newest first: {names}",
            quote(summary)
        ),
        cx.s.note_chars,
    )
}

/// Sends item `item`, if it is declined, back to triage once agents keep reporting it:
/// declined at least
/// `declined_reopen_after_days` ago, with `declined_reopen_reports` reports since then, on
/// it or on the items resolved as its duplicates (`CLUSTER_SINCE`), from
/// `declined_reopen_agents` agents that had not reported it before. It moves to `open`,
/// unowned and unresolved, with a note giving the counts since the decline. Every report
/// is a row of `feedback_reports`, and `resolved_report` is the newest row (of any item) at
/// the decline, so the counts do not depend on the capped `reporters` or on clock ties; a
/// later decline starts afresh. Runs on each report to a live item, and when a resolve
/// passes a duplicate's reports to one.
fn reopen_declined(cx: &mut Ctx<'_>, item: &TaskId, reporter: &AgentId, actor: &str) -> Result<()> {
    type Row = (Option<String>, Option<i64>, Option<i64>, Option<String>);
    let (kind, declined_at, last_report, reason): Row = cx.db.query_row(
        "SELECT resolution, resolved_at, resolved_report, resolution_reason FROM feedback \
         WHERE task = ?1",
        [item.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let (Some("declined"), Some(declined_at), Some(last_report)) =
        (kind.as_deref(), declined_at, last_report)
    else {
        return Ok(());
    };
    let now = cx.now_ms();
    if now - declined_at < cx.s.declined_reopen_ms() {
        return Ok(());
    }
    // A declined item is done: resolving moves it there, and reopening clears the
    // resolution.
    let owner: Option<String> = cx.db.query_row(
        "SELECT owner FROM tasks WHERE id = ?1",
        [item.as_str()],
        |r| r.get(0),
    )?;
    let (since, agents, new_agents): (u32, u32, u32) =
        cx.db.prepare_cached(CLUSTER_SINCE)?.query_row(
            params![
                item.as_str(),
                DUPLICATE_DEPTH_MAX as i64,
                last_report,
                declined_at
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    if since < cx.s.declined_reopen_reports || new_agents < cx.s.declined_reopen_agents {
        return Ok(());
    }
    reopen(cx, item)?;
    cx.db.execute(
        "UPDATE tasks SET status = 'open', owner = NULL, blocked_reason = NULL, \
         doing_since = NULL, done_at = NULL, summary = NULL, version = version + 1, \
         updated_at = ?2 WHERE id = ?1",
        params![item.as_str(), now],
    )?;
    let declined = ts(declined_at).format("%Y-%m-%d %H:%MZ");
    let text = cut(
        &format!(
            "reopened: {since} reports from {agents} agents since it was declined at \
             {declined} ({new_agents} agents new since then). Declined because: {}. Route it \
             to the setup's owner or ask the user.",
            reason
                .as_deref()
                .map_or("no reason given", |r| r.trim_end_matches('.'))
        ),
        cx.s.note_chars,
    );
    cx.db.execute(
        "INSERT INTO notes (task, agent, kind, text, at) VALUES (?1, ?2, 'finding', ?3, ?4)",
        params![item.as_str(), reporter.as_str(), text, now],
    )?;
    let note_id = cx.db.last_insert_rowid();
    let board = item.board();
    cx.emit(
        Some(&board),
        actor,
        EventType::NoteAdded,
        Some(item),
        serde_json::json!({ "note": note_id, "kind": "finding" }),
    )?;
    let mut payload = serde_json::json!({
        "fields": ["status", "feedback"],
        "from": "done",
        "to": "open",
        "reopened": { "reports": since, "agents": agents, "new_agents": new_agents },
    });
    // As `task_update` does when the owner changes.
    if let Some(owner) = owner {
        payload["fields"] = serde_json::json!(["status", "feedback", "owner"]);
        payload["owner"] = serde_json::Value::Null;
        payload["previous_owner"] = serde_json::json!(owner);
    }
    cx.emit(
        Some(&board),
        actor,
        EventType::TaskUpdated,
        Some(item),
        payload,
    )?;
    Ok(())
}

/// The feedback part of task `id`, if it is a feedback item. Its `reports`, `reporters`
/// and `last_reported_at` include those of the items resolved as its duplicates, and
/// theirs: counted when read, never copied, so each report keeps one row (the stats count
/// it once), and reopening a duplicate takes its reports back.
pub(crate) fn load(cx: &Ctx<'_>, id: &TaskId) -> Result<Option<Feedback>> {
    let rows = cx
        .db
        .prepare_cached(CLUSTER)?
        .query_map(params![id.as_str(), DUPLICATE_DEPTH_MAX as i64], |r| {
            let resolution = match r.get::<_, Option<String>>(8)? {
                Some(kind) => Some(FeedbackResolution {
                    kind: parse_enum(&kind)?,
                    task: r
                        .get::<_, Option<String>>(9)?
                        .map(|t| parse_id(&t))
                        .transpose()?,
                    reason: r.get(10)?,
                    at: ts(r.get(11)?),
                    by: r.get(12)?,
                }),
                None => None,
            };
            Ok((
                r.get::<_, String>(0)?,
                Feedback {
                    about: r.get(1)?,
                    activity: r
                        .get::<_, Option<String>>(2)?
                        .map(|a| parse_enum(&a))
                        .transpose()?,
                    source: parse_enum(&r.get::<_, String>(3)?)?,
                    signature: r.get(4)?,
                    reports: r.get(5)?,
                    reporters: parse_json(&r.get::<_, String>(6)?)?,
                    last_reported_at: ts(r.get(7)?),
                    resolution,
                    recurs: r
                        .get::<_, Option<String>>(13)?
                        .map(|t| parse_id(&t))
                        .transpose()?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let merged = rows.len() > 1;
    let mut reports = 0u32;
    // Agents listed by the items, in the order of the items' last reports.
    let mut listed: Vec<AgentId> = Vec::new();
    let mut last_reported_at = None;
    let mut own = None;
    for (task, mut feedback) in rows {
        reports = reports.saturating_add(feedback.reports);
        listed.extend(std::mem::take(&mut feedback.reporters));
        last_reported_at = last_reported_at.max(Some(feedback.last_reported_at));
        if task == id.as_str() {
            own = Some(feedback);
        }
    }
    let Some(mut feedback) = own else {
        return Ok(None);
    };
    let last_reported_at = last_reported_at.unwrap_or(feedback.last_reported_at);
    if !merged {
        feedback.reporters = listed;
        return Ok(Some(feedback));
    }
    // Across items, each agent's newest report row orders the reporters. An item adopted
    // from a plain task has no row for its creator: the agents only a list names go first.
    let by_row: Vec<AgentId> = cx
        .db
        .prepare_cached(CLUSTER_REPORTERS)?
        .query_map(params![id.as_str(), DUPLICATE_DEPTH_MAX as i64], |r| {
            parse_id(&r.get::<_, String>(0)?)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let listed_only: Vec<AgentId> = listed.into_iter().filter(|a| !by_row.contains(a)).collect();
    let mut reporters = Vec::new();
    for agent in listed_only.into_iter().chain(by_row) {
        push_reporter(&mut reporters, agent);
    }
    feedback.reports = reports;
    feedback.reporters = reporters;
    feedback.last_reported_at = last_reported_at;
    Ok(Some(feedback))
}

/// Adds `agent` as the newest of `reporters`, keeping the latest [`REPORTERS_MAX`].
fn push_reporter(reporters: &mut Vec<AgentId>, agent: AgentId) {
    reporters.retain(|a| a != &agent);
    reporters.push(agent);
    let excess = reporters.len().saturating_sub(REPORTERS_MAX);
    reporters.drain(..excess);
}

/// The CTE `cluster`: feedback item `?1` and the items resolved as its duplicates, and
/// theirs, at most `?2` links deep, as `live_item` follows them.
macro_rules! cluster {
    () => {
        "WITH RECURSIVE cluster (task, depth) AS (SELECT ?1, 0 UNION SELECT f.task, \
         c.depth + 1 FROM cluster c JOIN feedback f ON f.resolution_task = c.task \
         AND f.resolution = 'duplicate' WHERE c.depth < ?2) "
    };
}

/// The items of the cluster, oldest last report first.
pub(crate) const CLUSTER: &str = concat!(
    cluster!(),
    "SELECT task, about, activity, source, signature, reports, reporters, last_reported_at, \
     resolution, resolution_task, resolution_reason, resolved_at, resolved_by, recurs \
     FROM feedback WHERE task IN (SELECT task FROM cluster) ORDER BY last_reported_at, rowid"
);

/// The agents that reported the cluster, by their newest report, oldest first.
pub(crate) const CLUSTER_REPORTERS: &str = concat!(
    cluster!(),
    "SELECT agent FROM feedback_reports WHERE task IN (SELECT task FROM cluster) \
     GROUP BY agent ORDER BY MAX(id)"
);

/// The cluster's reports that are API errors.
pub(crate) const CLUSTER_ERRORS: &str = concat!(
    cluster!(),
    "SELECT COUNT(*) FROM feedback_reports WHERE signal = 1 \
     AND task IN (SELECT task FROM cluster)"
);

/// The cluster's reports since a decline at `?4`, when report row `?3` was the newest:
/// their count, their agents, and the agents new since then. A report counts when its row
/// came after `?3` and its time is not before `?4`, since a signal item's rows copy errors
/// from before it was filed. An agent counts as new when it had not reported the problem
/// by then, on any item of the cluster, nor created one of them: the creator reported it
/// first, even on an item adopted from a plain task, which has no row.
const CLUSTER_SINCE: &str = concat!(
    cluster!(),
    ", report_rows AS (SELECT agent, id > ?3 AND at >= ?4 AS since FROM feedback_reports \
     WHERE task IN (SELECT task FROM cluster)), \
     earlier (agent) AS (SELECT agent FROM report_rows WHERE NOT since UNION \
     SELECT created_by FROM tasks WHERE id IN (SELECT task FROM cluster) \
     AND created_at <= ?4) \
     SELECT COUNT(*), COUNT(DISTINCT agent), COUNT(DISTINCT CASE WHEN agent NOT IN \
     (SELECT agent FROM earlier) THEN agent END) FROM report_rows WHERE since"
);

/// The newest report of feedback item `?1` and of the items its duplicate links lead to,
/// at most `?2` links on, as `live_item` follows them.
const CHAIN_LAST_REPORTED: &str = "WITH RECURSIVE chain (task, depth) AS (SELECT ?1, 0 \
     UNION SELECT f.resolution_task, c.depth + 1 FROM chain c JOIN feedback f \
     ON f.task = c.task AND f.resolution = 'duplicate' WHERE c.depth < ?2) \
     SELECT MAX(last_reported_at) FROM feedback WHERE task IN (SELECT task FROM chain)";

/// Forgets the resolution of a reopened item, so it is live again.
pub(crate) fn reopen(cx: &Ctx<'_>, id: &TaskId) -> Result<()> {
    cx.db.execute(
        "UPDATE feedback SET resolution = NULL, resolution_task = NULL, \
         resolution_reason = NULL, resolved_at = NULL, resolved_by = NULL, \
         resolved_report = NULL WHERE task = ?1",
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
        if req.kind == ResolutionKind::Duplicate {
            check_original(cx, id, target)?;
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
         resolved_at = ?5, resolved_by = ?6, \
         resolved_report = (SELECT COALESCE(MAX(id), 0) FROM feedback_reports) WHERE task = ?1",
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
    // The checks above see `id` still open. Once it is resolved, a chain of duplicates that
    // leads back to it (or runs past the depth limit) leaves it taking no reports; the
    // error rolls the resolution back.
    let mut takes_reports = None;
    if req.kind == ResolutionKind::Duplicate
        && let Some(original) = &req.task
    {
        let Some(live) = live_item(cx, id, 0)? else {
            return Err(invalid(
                format!(
                    "{original} passes its reports back to {id}, or on through more than \
                     {DUPLICATE_DEPTH_MAX} duplicate links, so {id} cannot be its duplicate"
                ),
                format!(
                    "name the item that takes the reports, at the end of the duplicate links, \
                     not one that leads back to {id}"
                ),
            ));
        };
        // A duplicate of a plain task keeps its own reports.
        takes_reports = (&live != id).then_some(live);
    }
    crate::ops::leases::release_for_task(cx, id, &task.board, &by)?;
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
    // The item's reports now count on the original, which announces them as a repeat
    // report does, crediting the item's latest reporter and quoting its title.
    if let Some(live) = takes_reports {
        let feedback = load(cx, id)?.expect("adopted above if it was plain");
        let author = feedback
            .reporters
            .last()
            .cloned()
            .unwrap_or_else(|| task.created_by.clone());
        let signal = feedback.source == FeedbackSource::Signal;
        note_reports(cx, &live, &author, &by, &task.title, signal)?;
        // Its reports since a decline count toward sending a declined original back.
        reopen_declined(cx, &live, &author, &by)?;
    }
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
            "send the server's message for it, such as text is 801 characters; the limit \
             is 800, for a field clients check (docs/API.md)",
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

/// Whether an error answered to `route` (method and route template) is an expected answer,
/// not friction: a version conflict or one of [`EXPECTED_ERRORS`]. Such errors are never
/// recorded, so callers may skip them before doing any work to record them.
pub(crate) fn is_expected(code: ErrorCode, route: &str, message: &str) -> bool {
    code == ErrorCode::VersionConflict
        || EXPECTED_ERRORS
            .iter()
            .any(|(c, r, start)| *c == code && *r == route && message.starts_with(start))
}

/// Records an error answered to an agent, unless it [`is_expected`]. Once its signature
/// reaches the signal threshold it files feedback (source `signal`); later ones add to that
/// item while it is live.
pub(crate) fn record_error(cx: &mut Ctx<'_>, e: ApiErrorSeen) -> Result<Option<TaskId>> {
    if is_expected(e.code, &e.route, &e.message) {
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
            add_report(cx, &item, &e.agent, &summary, true)?;
            return Ok(Some(item));
        }
        Prior::Gone(earlier) => Some(earlier),
        Prior::None => None,
    };
    // Records outlive the signal window (error_retention_days), so only those within it
    // count. Errors that an earlier item took (before its fix) do not count towards a new
    // one, nor do those it passed on to the item its duplicate links lead to.
    let mut from = now - cx.s.signal_window_ms();
    if let Some(earlier) = &earlier {
        let last: i64 = cx
            .db
            .prepare_cached(CHAIN_LAST_REPORTED)?
            .query_row(params![earlier.as_str(), DUPLICATE_DEPTH_MAX as i64], |r| {
                r.get(0)
            })?;
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
        "INSERT INTO feedback_reports (task, agent, at, signal) SELECT ?1, agent, at, 1 FROM api_errors \
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
    fn expected_errors_match_by_code_route_and_message_start() {
        let claim = "POST /v1/tasks/{id}/claim";
        let cases = [
            (ErrorCode::VersionConflict, "POST /v1/tasks/{id}", "", true),
            (ErrorCode::NotFound, "GET /v1/agents/{id}", "no agent", true),
            (
                ErrorCode::NotFound,
                "GET /v1/agents/{id}/handoff",
                "x",
                true,
            ),
            (ErrorCode::NotFound, "GET /v1/tasks/{id}", "no task", false),
            (
                ErrorCode::ClaimRefused,
                claim,
                "claim refused: t3:a owns it",
                true,
            ),
            (
                ErrorCode::ClaimRefused,
                claim,
                "you already have x-1 in doing",
                false,
            ),
            (ErrorCode::Invalid, claim, "claim refused: x", false),
            (
                ErrorCode::ClaimRefused,
                "POST /v1/boards/{slug}/coordinator",
                "held by another device",
                true,
            ),
        ];
        for (code, route, message, expected) in cases {
            assert_eq!(
                is_expected(code, route, message),
                expected,
                "{code:?} {route} {message:?}"
            );
        }
    }

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
