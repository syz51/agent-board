use board_proto::{
    Board, BoardSlug, BoardStatus, ClaimCoordinator, CreateBoard, DeviceName, EventType,
    UpdateBoard,
};
use rusqlite::{OptionalExtension, params};

use crate::config::Settings;
use crate::db::{Ctx, enum_str};
use crate::error::{Result, forbidden, invalid, refused};
use crate::ops::{expires, join_board, touch_agent};
use crate::store::{board_missing, get_board};
use crate::text::{ago, check_line, check_text, normalize_repo};

fn check_repos(repos: Vec<String>) -> Result<Vec<String>> {
    if repos.len() > 20 {
        return Err(invalid(
            "a board covers at most 20 repos",
            "split the work into several boards",
        ));
    }
    let mut out = Vec::with_capacity(repos.len());
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
    let mut out: Vec<DeviceName> = Vec::with_capacity(devices.len());
    for device in devices {
        if !out.contains(&device) {
            out.push(device);
        }
    }
    Ok(out)
}

pub(crate) fn validate_create(mut req: CreateBoard, s: &Settings) -> Result<CreateBoard> {
    check_line("title", &req.title, s.title_chars)?;
    check_text("goal", &req.goal, s.body_chars)?;
    req.repos = check_repos(req.repos)?;
    req.home_devices = check_devices(req.home_devices)?;
    Ok(req)
}

pub(crate) fn validate_update(mut req: UpdateBoard, s: &Settings) -> Result<UpdateBoard> {
    if let Some(title) = &req.title {
        check_line("title", title, s.title_chars)?;
    }
    if let Some(goal) = &req.goal {
        check_text("goal", goal, s.body_chars)?;
    }
    req.repos = req.repos.map(check_repos).transpose()?;
    req.home_devices = req.home_devices.map(check_devices).transpose()?;
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
    cx.db.execute(
        "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
         created_at, updated_at) VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6, ?7, ?7)",
        params![
            req.slug.as_str(),
            req.title,
            req.goal,
            serde_json::to_string(&req.repos).expect("strings serialize"),
            req.autopilot,
            serde_json::to_string(&req.home_devices).expect("strings serialize"),
            now,
        ],
    )?;
    cx.emit(
        Some(&req.slug),
        actor,
        EventType::BoardCreated,
        None,
        serde_json::json!({ "title": req.title, "repos": req.repos, "autopilot": req.autopilot }),
    )?;
    get_board(cx, &req.slug)
}

pub(crate) fn update(
    cx: &mut Ctx<'_>,
    actor: &str,
    slug: &BoardSlug,
    req: UpdateBoard,
) -> Result<Board> {
    let before = get_board(cx, slug)?;
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
    if fields.is_empty() {
        return Ok(before);
    }
    cx.db.execute(
        "UPDATE boards SET title = ?2, goal = ?3, status = ?4, repos = ?5, autopilot = ?6, \
         home_devices = ?7, updated_at = ?8 WHERE slug = ?1",
        params![
            slug.as_str(),
            req.title.unwrap_or(before.title),
            req.goal.unwrap_or(before.goal),
            enum_str(&req.status.unwrap_or(before.status)),
            serde_json::to_string(&req.repos.unwrap_or(before.repos)).expect("serializes"),
            req.autopilot.unwrap_or(before.autopilot),
            serde_json::to_string(&req.home_devices.unwrap_or(before.home_devices))
                .expect("serializes"),
            cx.now_ms(),
        ],
    )?;
    cx.emit(
        Some(slug),
        actor,
        EventType::BoardUpdated,
        None,
        serde_json::json!({ "fields": fields }),
    )?;
    get_board(cx, slug)
}

pub(crate) fn join(
    cx: &mut Ctx<'_>,
    slug: &BoardSlug,
    agent: &board_proto::AgentId,
) -> Result<Board> {
    get_board(cx, slug)?;
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
    if let Some(agent) = &req.agent {
        touch_agent(cx, agent)?;
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
        serde_json::json!({ "fields": ["coordinator"], "coordinator": null }),
    )?;
    get_board(cx, slug)
}
