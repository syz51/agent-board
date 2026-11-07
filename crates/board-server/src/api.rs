//! The `/v1` HTTP API (`docs/API.md`): routing, request validation and authorization.
//! Rules live in `ops`; handlers check who may act, validate input, then run one
//! transaction.

use axum::extract::{FromRequest, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use board_proto::{
    AddNote, Agent, AgentId, AgentState, AgentStateUpdate, AnswerAsk, Ask, AskStatus, Board,
    BoardSlug, ClaimCoordinator, CreateAsk, CreateBoard, CreateDevice, CreateLeases, CreateTask,
    DeviceInfo, DeviceName, DeviceToken, DoneTask, ErrorCode, Handoff, LeaseResult, Note,
    PutHandoff, RegisterAgent, Snapshot, SnapshotFormat, SnapshotKind, Stats, Task, TaskAction,
    TaskDetail, TaskStatus, TouchResult, Touches, UpdateBoard, UpdateTask,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::auth::Caller;
use crate::error::{Error, Result, invalid, not_found};
use crate::ops::tasks::{TaskFilter, parse_task_id};
use crate::ops::{agents, asks, boards, devices, leases, stats, tasks};
use crate::state::AppState;
use crate::store::{find_handoff, get_agent, get_ask, get_board, list_boards, task_detail};
use crate::{events, snapshot, web};

/// A JSON body whose rejection is an [`ApiError`](board_proto::ApiError).
pub(crate) struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = Error;

    async fn from_request(req: Request, state: &S) -> Result<Self> {
        let Json(value) = Json::<T>::from_request(req, state).await?;
        Ok(Self(value))
    }
}

/// Query parameters whose rejection is an [`ApiError`](board_proto::ApiError).
pub(crate) struct Q<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Q<T> {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let axum::extract::Query(value) =
            axum::extract::Query::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

fn parse_path<T: std::str::FromStr<Err = board_proto::InvalidId>>(raw: &str) -> Result<T> {
    raw.parse()
        .map_err(|e: board_proto::InvalidId| not_found(e.to_string(), "check the id in the path"))
}

pub(crate) fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/agents", post(register_agent).get(list_agents))
        .route("/agents/{id}", get(show_agent))
        .route("/agents/{id}/state", post(agent_state))
        .route("/agents/{id}/handoff", get(show_handoff).put(put_handoff))
        .route("/boards", get(list_boards_h).post(create_board))
        .route(
            "/boards/{slug}",
            get(show_board).patch(update_board).delete(delete_board),
        )
        .route("/boards/{slug}/join", post(join_board))
        .route(
            "/boards/{slug}/coordinator",
            post(claim_coordinator).delete(release_coordinator),
        )
        .route("/stats", get(show_stats))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{id}", get(show_task).patch(update_task))
        .route("/tasks/{id}/claim", post(claim_task))
        .route("/tasks/{id}/release", post(release_task))
        .route("/tasks/{id}/take", post(take_task))
        .route("/tasks/{id}/done", post(done_task))
        .route("/tasks/{id}/notes", post(add_note))
        .route("/leases", post(create_leases))
        .route("/leases/{id}", delete(release_lease))
        .route("/touches", post(touches))
        .route("/asks", get(list_asks).post(create_ask))
        .route("/asks/{id}", get(show_ask))
        .route("/asks/{id}/answer", post(answer_ask))
        .route("/asks/{id}/withdraw", post(withdraw_ask))
        .route("/snapshot", get(show_snapshot))
        .route("/events", get(show_events))
        .route("/admin/devices", get(list_devices).post(create_device))
        .route("/admin/devices/{name}", delete(revoke_device))
        .fallback(unknown_api);
    Router::new()
        .route("/healthz", get(health))
        .nest(board_proto::API_PREFIX, v1)
        .fallback(web::serve)
        .with_state(state)
}

async fn unknown_api() -> Error {
    not_found(
        "no such endpoint",
        "check the method and path against docs/API.md",
    )
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let seq = state.read(|cx| cx.max_seq()).await?;
    Ok(Json(serde_json::json!({ "ok": true, "seq": seq })))
}

// ---------------------------------------------------------------------------------------
// Agents

async fn register_agent(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<RegisterAgent>,
) -> Result<Json<Agent>> {
    caller.act_as(&req.id)?;
    caller.act_for_device(&req.device)?;
    let reg = agents::validate_register(req)?;
    Ok(Json(
        state.write(move |cx| agents::register(cx, reg)).await?,
    ))
}

#[derive(Deserialize)]
struct AgentsQuery {
    board: Option<BoardSlug>,
    state: Option<AgentState>,
}

async fn list_agents(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<AgentsQuery>,
) -> Result<Json<Vec<Agent>>> {
    Ok(Json(
        state
            .read(move |cx| agents::list(cx, q.board.as_ref(), q.state))
            .await?,
    ))
}

async fn show_agent(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Agent>> {
    let id: AgentId = parse_path(&id)?;
    Ok(Json(state.read(move |cx| get_agent(cx, &id)).await?))
}

async fn agent_state(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(update): Body<AgentStateUpdate>,
) -> Result<Json<Agent>> {
    let id: AgentId = parse_path(&id)?;
    caller.act_as(&id)?;
    agents::validate_state(&update)?;
    Ok(Json(
        state
            .write(move |cx| agents::set_state(cx, &id, update))
            .await?,
    ))
}

async fn show_handoff(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Handoff>> {
    let id: AgentId = parse_path(&id)?;
    let handoff = state
        .read(move |cx| {
            get_agent(cx, &id)?;
            find_handoff(cx, &id)?.ok_or_else(|| {
                not_found(
                    format!("{id} has no handoff"),
                    "write one with handoff_set (PUT /v1/agents/{id}/handoff)",
                )
            })
        })
        .await?;
    Ok(Json(handoff))
}

async fn put_handoff(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<PutHandoff>,
) -> Result<Json<Handoff>> {
    let id: AgentId = parse_path(&id)?;
    caller.act_as(&id)?;
    agents::validate_handoff(&req, state.settings().handoff_field_chars)?;
    Ok(Json(
        state
            .write(move |cx| agents::put_handoff(cx, &id, req))
            .await?,
    ))
}

// ---------------------------------------------------------------------------------------
// Boards

async fn list_boards_h(State(state): State<AppState>, _caller: Caller) -> Result<Json<Vec<Board>>> {
    Ok(Json(state.read(list_boards).await?))
}

async fn create_board(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateBoard>,
) -> Result<Json<Board>> {
    let req = boards::validate_create(req, state.settings())?;
    let actor = caller.actor();
    Ok(Json(
        state
            .write(move |cx| boards::create(cx, &actor, req))
            .await?,
    ))
}

async fn show_board(
    State(state): State<AppState>,
    _caller: Caller,
    Path(slug): Path<String>,
) -> Result<Json<Board>> {
    let slug: BoardSlug = parse_path(&slug)?;
    Ok(Json(state.read(move |cx| get_board(cx, &slug)).await?))
}

async fn update_board(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
    Body(req): Body<UpdateBoard>,
) -> Result<Json<Board>> {
    let slug: BoardSlug = parse_path(&slug)?;
    let req = boards::validate_update(req, state.settings())?;
    let actor = caller.actor();
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| boards::update(cx, &actor, &slug, req, by_user))
            .await?,
    ))
}

/// User only: deleting cannot be undone.
async fn delete_board(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
) -> Result<StatusCode> {
    caller.require_user()?;
    let slug: BoardSlug = parse_path(&slug)?;
    let actor = caller.actor();
    state
        .write(move |cx| boards::delete(cx, &actor, &slug))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn join_board(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
    Body(req): Body<board_proto::JoinBoard>,
) -> Result<Json<Board>> {
    let slug: BoardSlug = parse_path(&slug)?;
    caller.act_as(&req.agent)?;
    Ok(Json(
        state
            .write(move |cx| boards::join(cx, &slug, &req.agent))
            .await?,
    ))
}

async fn claim_coordinator(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
    Body(req): Body<ClaimCoordinator>,
) -> Result<Json<Board>> {
    let slug: BoardSlug = parse_path(&slug)?;
    if let Some(agent) = &req.agent {
        caller.act_as(agent)?;
    }
    caller.act_for_device(&req.device)?;
    let actor = caller.actor();
    Ok(Json(
        state
            .write(move |cx| boards::claim_coordinator(cx, &actor, &slug, req))
            .await?,
    ))
}

async fn release_coordinator(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
) -> Result<Json<Board>> {
    let slug: BoardSlug = parse_path(&slug)?;
    let actor = caller.actor();
    let device = caller.device().cloned();
    let is_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| {
                boards::release_coordinator(cx, &actor, &slug, device.as_ref(), is_user)
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct StatsQuery {
    days: Option<u32>,
}

async fn show_stats(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<StatsQuery>,
) -> Result<Json<Stats>> {
    let days = stats::validate_days(q.days)?;
    Ok(Json(state.read(move |cx| stats::stats(cx, days)).await?))
}

// ---------------------------------------------------------------------------------------
// Tasks

#[derive(Deserialize)]
struct TasksQuery {
    board: Option<BoardSlug>,
    status: Option<TaskStatus>,
    owner: Option<AgentId>,
    ready: Option<bool>,
    mine: Option<bool>,
    limit: Option<usize>,
}

async fn list_tasks(
    State(state): State<AppState>,
    caller: Caller,
    Q(q): Q<TasksQuery>,
) -> Result<Json<Vec<Task>>> {
    let owner = if q.mine == Some(true) {
        let me = caller.agent()?.clone();
        if q.owner.as_ref().is_some_and(|o| *o != me) {
            return Err(invalid(
                "mine=true and owner name different agents",
                "send either mine=true or owner=<agent id>",
            ));
        }
        Some(me)
    } else {
        q.owner
    };
    let filter = TaskFilter {
        board: q.board,
        status: q.status,
        owner,
        ready: q.ready,
        limit: q.limit.unwrap_or(200).clamp(1, 1000),
    };
    Ok(Json(state.read(move |cx| tasks::list(cx, filter)).await?))
}

async fn create_task(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateTask>,
) -> Result<Json<Task>> {
    caller.act_as(&req.agent)?;
    tasks::validate_create(&req, state.settings())?;
    Ok(Json(state.write(move |cx| tasks::create(cx, req)).await?))
}

async fn show_task(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>> {
    let id = parse_task_id(&id)?;
    Ok(Json(state.read(move |cx| task_detail(cx, &id)).await?))
}

async fn update_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<UpdateTask>,
) -> Result<Json<Task>> {
    let id = parse_task_id(&id)?;
    caller.act_as(&req.agent)?;
    tasks::validate_update(&req, state.settings())?;
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| tasks::update(cx, &id, req, by_user))
            .await?,
    ))
}

async fn claim_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<TaskAction>,
) -> Result<Json<Task>> {
    let id = parse_task_id(&id)?;
    caller.act_as(&req.agent)?;
    let (task, agent) = (id.clone(), req.agent.clone());
    let result = state.write(move |cx| tasks::claim(cx, &task, &agent)).await;
    if let Err(error) = &result
        && error.code() == Some(ErrorCode::ClaimRefused)
    {
        state
            .write(move |cx| tasks::record_refusal(cx, &id, &req.agent))
            .await?;
    }
    Ok(Json(result?))
}

async fn release_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<TaskAction>,
) -> Result<Json<Task>> {
    let id = parse_task_id(&id)?;
    caller.act_as(&req.agent)?;
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| tasks::release(cx, &id, &req.agent, by_user))
            .await?,
    ))
}

async fn take_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<TaskAction>,
) -> Result<Json<Task>> {
    let id = parse_task_id(&id)?;
    caller.act_as(&req.agent)?;
    Ok(Json(
        state
            .write(move |cx| tasks::take(cx, &id, &req.agent))
            .await?,
    ))
}

async fn done_task(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<DoneTask>,
) -> Result<Json<Task>> {
    let id = parse_task_id(&id)?;
    caller.act_as(&req.agent)?;
    tasks::validate_done(&req, &id)?;
    Ok(Json(
        state.write(move |cx| tasks::done(cx, &id, req)).await?,
    ))
}

async fn add_note(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<AddNote>,
) -> Result<Json<Note>> {
    let id = parse_task_id(&id)?;
    caller.act_as(&req.agent)?;
    tasks::validate_note(&req, state.settings())?;
    Ok(Json(
        state.write(move |cx| tasks::add_note(cx, &id, req)).await?,
    ))
}

// ---------------------------------------------------------------------------------------
// Leases and touches

async fn create_leases(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateLeases>,
) -> Result<Json<LeaseResult>> {
    caller.act_as(&req.agent)?;
    let valid = leases::validate_create(req)?;
    Ok(Json(
        state.write(move |cx| leases::create(cx, valid)).await?,
    ))
}

async fn release_lease(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    let agent = caller.agent()?.clone();
    state
        .write(move |cx| leases::release(cx, id, &agent))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn touches(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<Touches>,
) -> Result<Json<TouchResult>> {
    caller.act_as(&req.agent)?;
    let req = leases::validate_touches(req)?;
    Ok(Json(state.write(move |cx| leases::touch(cx, req)).await?))
}

// ---------------------------------------------------------------------------------------
// Asks

#[derive(Deserialize)]
struct AsksQuery {
    status: Option<AskStatus>,
    board: Option<BoardSlug>,
}

async fn list_asks(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<AsksQuery>,
) -> Result<Json<Vec<Ask>>> {
    Ok(Json(
        state
            .read(move |cx| asks::list(cx, q.status, q.board.as_ref()))
            .await?,
    ))
}

async fn show_ask(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<i64>,
) -> Result<Json<Ask>> {
    Ok(Json(state.read(move |cx| get_ask(cx, id)).await?))
}

async fn create_ask(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateAsk>,
) -> Result<Json<Ask>> {
    caller.act_as(&req.agent)?;
    asks::validate_create(&req, state.settings())?;
    Ok(Json(state.write(move |cx| asks::create(cx, req)).await?))
}

async fn answer_ask(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<i64>,
    Body(req): Body<AnswerAsk>,
) -> Result<Json<Ask>> {
    caller.require_user()?;
    asks::validate_answer(&req, state.settings())?;
    let via = caller.answered_via();
    Ok(Json(
        state
            .write(move |cx| asks::answer(cx, id, &req.answer, via))
            .await?,
    ))
}

async fn withdraw_ask(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<i64>,
    Body(req): Body<TaskAction>,
) -> Result<Json<Ask>> {
    caller.act_as(&req.agent)?;
    Ok(Json(
        state
            .write(move |cx| asks::withdraw(cx, id, &req.agent))
            .await?,
    ))
}

// ---------------------------------------------------------------------------------------
// Snapshots and events

#[derive(Deserialize)]
struct SnapshotQuery {
    agent: Option<AgentId>,
    kind: Option<SnapshotKind>,
    since: Option<i64>,
    format: Option<SnapshotFormat>,
}

async fn show_snapshot(
    State(state): State<AppState>,
    caller: Caller,
    Q(q): Q<SnapshotQuery>,
) -> Result<Json<Snapshot>> {
    let agent = match (q.agent, &caller.agent) {
        (Some(agent), Some(_)) => {
            caller.act_as(&agent)?;
            agent
        }
        (None, Some(header)) => header.clone(),
        (Some(agent), None) if caller.is_user() => agent,
        (_, None) => {
            return Err(invalid(
                "X-Board-Agent is missing",
                "send the header X-Board-Agent: <your agent id>",
            ));
        }
    };
    // Only the agent itself moves its cursor; the user may look without disturbing it.
    let advance = caller.agent.as_ref() == Some(&agent);
    let req = snapshot::Request {
        agent,
        kind: q.kind.unwrap_or(SnapshotKind::Full),
        since: q.since,
        format: q.format.unwrap_or_default(),
        advance,
    };
    Ok(Json(
        state.write(move |cx| snapshot::snapshot(cx, req)).await?,
    ))
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    board: Option<BoardSlug>,
    limit: Option<u32>,
}

async fn show_events(
    State(state): State<AppState>,
    _caller: Caller,
    headers: HeaderMap,
    Q(q): Q<EventsQuery>,
) -> Result<Response> {
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if wants_stream {
        let resume = headers
            .get("last-event-id")
            .map(|v| {
                v.to_str()
                    .ok()
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .ok_or_else(|| {
                        invalid(
                            "Last-Event-ID is not an event sequence number",
                            "send the id of the last event received, or leave it out",
                        )
                    })
            })
            .transpose()?;
        let since = resume.or(q.since).unwrap_or(0);
        return Ok(events::stream(state, since, q.board).into_response());
    }
    let since = q.since.unwrap_or(0);
    let limit = q
        .limit
        .unwrap_or(events::DEFAULT_LIMIT)
        .clamp(1, events::MAX_LIMIT);
    let page = state
        .read(move |cx| events::page(cx, since, q.board.as_ref(), limit))
        .await?;
    Ok(Json(page).into_response())
}

// ---------------------------------------------------------------------------------------
// Administration

async fn create_device(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateDevice>,
) -> Result<Json<DeviceToken>> {
    caller.require_user()?;
    Ok(Json(
        state
            .write(move |cx| devices::create(cx, &req.name, false))
            .await?,
    ))
}

async fn list_devices(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<DeviceInfo>>> {
    caller.require_user()?;
    Ok(Json(state.read(devices::list).await?))
}

async fn revoke_device(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    caller.require_user()?;
    let name: DeviceName = parse_path(&name)?;
    state.write(move |cx| devices::revoke(cx, &name)).await?;
    Ok(StatusCode::NO_CONTENT)
}
