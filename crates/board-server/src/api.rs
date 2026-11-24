//! The `/v1` HTTP API (`docs/API.md`): routing, request validation and authorization.
//! Rules live in `ops`; handlers check who may act, validate input, then run one
//! transaction.

use axum::extract::{FromRequest, FromRequestParts, MatchedPath, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use board_proto::{
    AGENT_HEADER, AddNote, AdminUpdateDevice, Agent, AgentId, AgentState, AgentStateUpdate,
    AnswerAsk, ApiError, ApprovedBoardChange, Ask, AskStatus, Board, BoardSlug, CancelRunRequest,
    CancelRunResponse, ClaimCoordinator, ClientError, CreateAsk, CreateBoard, CreateDevice,
    CreateFeedback, CreateLeases, CreateRun, CreateTask, CreatedAsk, DeviceEntry, DeviceHeartbeat,
    DeviceInfo, DeviceName, DeviceToken, DispatchClaimRequest, DispatchClaimResponse, DoneTask,
    ErrorCode, ErrorLog, FeedbackResult, Handoff, Judge, LeaseResult, MarkSeen, Note,
    PushRunEvents, PutHandoff, PutSchedulerPolicy, PutUsageBucket, PutUsageBucketReserve,
    RUN_EVENTS_LIMIT_DEFAULT, RUN_EVENTS_LIMIT_MAX, RankTasks, RankedTasks, RegisterAgent,
    RegisterJudge, ReportUsage, ResolveFeedback, Run, RunEventRecord, RunFilter, RunId, Said,
    SchedulerPolicy, SchedulerPolicyResponse, SchedulerStats, SeenCursor, Snapshot, SnapshotFormat,
    SnapshotKind, Stats, SteerRunRequest, SteerRunResponse, StickyWake, Task, TaskAction,
    TaskDetail, TaskId, TaskStatus, Timestamp, TouchResult, Touches, UpdateBoard, UpdateRun,
    UpdateTask, Updates, Usage, UsageBucket, UsageReserve,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::auth::Caller;
use crate::error::{Error, Result, forbidden, invalid, not_found};
use crate::ops::tasks::{TaskFilter, parse_task_id};
use crate::ops::{
    agents, asks, boards, devices, dispatch, feedback, judges, leases, policy, runs, stats, tasks,
    updates, usage,
};
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
        .route("/agents/{id}/said", get(list_said))
        .route("/updates", get(show_updates))
        .route("/updates/seen", put(mark_seen))
        .route("/boards", get(list_boards_h).post(create_board))
        .route(
            "/boards/{slug}",
            get(show_board).patch(update_board).delete(delete_board),
        )
        .route("/boards/{slug}/join", post(join_board))
        .route("/boards/{slug}/approved", post(approved_board_change))
        .route("/boards/{slug}/rank", post(rank_tasks))
        .route(
            "/boards/{slug}/coordinator",
            post(claim_coordinator).delete(release_coordinator),
        )
        .route(
            "/boards/{slug}/judges",
            get(list_judges).post(register_judge),
        )
        .route("/stats", get(show_stats))
        .route("/stats/scheduler", get(show_scheduler_stats))
        .route(
            "/scheduler/policy",
            get(show_scheduler_policy).put(put_scheduler_policy),
        )
        .route("/usage", get(show_usage).put(report_usage))
        .route("/usage/reserve", get(show_reserve).put(set_reserve))
        .route("/usage/buckets", get(list_usage_buckets))
        .route("/usage/{provider}/{account}", put(put_usage_bucket))
        .route(
            "/usage/{provider}/{account}/reserve",
            put(put_usage_bucket_reserve),
        )
        .route("/devices", get(list_device_entries))
        .route("/devices/{name}/heartbeat", put(device_heartbeat))
        .route("/devices/{name}/wakes", get(list_device_wakes))
        .route("/wakes/{id}/ack", post(ack_wake))
        .route("/dispatch/claim", post(dispatch_claim))
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{id}", get(show_run).patch(update_run))
        .route(
            "/runs/{id}/events",
            get(show_run_events).post(push_run_events),
        )
        .route("/runs/{id}/steer", post(steer_run))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/errors", get(show_errors).post(report_error))
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
        .route("/feedback", post(create_feedback))
        .route("/feedback/{id}/resolve", post(resolve_feedback))
        .route("/asks", get(list_asks).post(create_ask))
        .route("/asks/{id}", get(show_ask))
        .route("/asks/{id}/answer", post(answer_ask))
        .route("/asks/{id}/withdraw", post(withdraw_ask))
        .route("/snapshot", get(show_snapshot))
        .route("/events", get(show_events))
        .route("/admin/devices", get(list_devices).post(create_device))
        .route(
            "/admin/devices/{name}",
            patch(admin_update_device).delete(revoke_device),
        )
        .fallback(unknown_api)
        .layer(middleware::from_fn_with_state(state.clone(), record_errors));
    Router::new()
        .route("/healthz", get(health))
        .nest(board_proto::API_PREFIX, v1)
        .fallback(web::serve)
        .layer(middleware::map_response_with_state(
            state.clone(),
            build_headers,
        ))
        .with_state(state)
}

/// Names this build on every answer, so clients can warn when they and the server differ.
async fn build_headers(State(state): State<AppState>, mut resp: Response) -> Response {
    let headers = resp.headers_mut();
    for (name, value) in state.build_headers() {
        headers.insert(name.clone(), value.clone());
    }
    resp
}

async fn unknown_api() -> Error {
    not_found(
        "no such endpoint",
        "check the method and path against docs/API.md",
    )
}

/// Records every error answered to an agent's request (one that sends `X-Board-Agent`),
/// except expected ones (`ops::feedback::is_expected`), for the feedback signals
/// (`ops::feedback::record_error`). Expected errors are skipped before the credentials are
/// checked, so they cost no read or write. Recording never changes the answer; a failure
/// to record is logged.
async fn record_errors(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let named_agent = req.headers().contains_key(AGENT_HEADER);
    let headers = named_agent.then(|| req.headers().clone());
    let route = format!(
        "{} {}",
        req.method(),
        req.extensions()
            .get::<MatchedPath>()
            .map_or("(no route)", MatchedPath::as_str)
    );
    let response = next.run(req).await;
    if let Some(headers) = headers
        && let Some(error) = response.extensions().get::<ApiError>()
        && !feedback::is_expected(error.code, &route, &error.message)
        // Only authenticated callers: a request without a valid token never writes.
        && let Some(agent) = authenticated_agent(&state, headers).await
    {
        let seen = feedback::ApiErrorSeen {
            agent,
            code: error.code,
            route,
            message: error.message.clone(),
            source: feedback::ErrorSource::Server,
        };
        if let Err(error) = state
            .write(move |cx| feedback::record_error(cx, seen))
            .await
        {
            tracing::warn!(%error, "recording an API error for the feedback signals failed");
        }
    }
    response
}

/// The agent a request speaks for, once its credentials check out as for a handler.
async fn authenticated_agent(state: &AppState, headers: HeaderMap) -> Option<AgentId> {
    let mut request = Request::new(());
    *request.headers_mut() = headers;
    let (mut parts, ()) = request.into_parts();
    let caller = Caller::from_request_parts(&mut parts, state).await.ok()?;
    caller.agent().ok().cloned()
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let commit = state.commit().to_owned();
    let (seq, schema) = state
        .read(|cx| {
            let schema: i64 = cx
                .db
                .pragma_query_value(None, "user_version", |r| r.get(0))?;
            Ok((cx.max_seq()?, schema))
        })
        .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "seq": seq,
        "version": board_proto::VERSION,
        "commit": commit,
        "schema_version": schema,
        "features": board_proto::SERVER_FEATURES.split(',').collect::<Vec<_>>(),
    })))
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
    let s = state.settings();
    agents::validate_handoff(&req, s.handoff_now_max(), s.handoff_field_chars)?;
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

/// Applies a change to the settings only the user may raise, which the user approved in
/// their own words. It takes the user's login: an agent runs it on a device that holds one
/// (`board approved board update`), and the event names the agent, the device and the words.
async fn approved_board_change(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
    Body(req): Body<ApprovedBoardChange>,
) -> Result<Json<Board>> {
    if !caller.is_user() {
        return Err(forbidden(
            "only the user's login may apply an approved board change",
            "run `board approved board update` where the user home holds a login with a \
             token created with --user, or ask the user with the change attached (ask_user \
             with change)",
        ));
    }
    let slug: BoardSlug = parse_path(&slug)?;
    boards::validate_approved(&req)?;
    boards::validate_change(&req.change, state.settings())?;
    // The event must name who used the user's login: a registered agent, never nobody.
    let agent = caller.agent()?.clone();
    let device = caller.device().cloned();
    Ok(Json(
        state
            .write(move |cx| {
                get_agent(cx, &agent)?;
                let approval = boards::Approval::Said {
                    said: &req.said,
                    device: device.as_ref(),
                };
                boards::update_approved(cx, agent.as_str(), &slug, &req.change, approval)
            })
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

/// The board's coordinator registers a judge; the agent comes from `X-Board-Agent`.
async fn register_judge(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
    Body(req): Body<RegisterJudge>,
) -> Result<Json<Judge>> {
    let slug: BoardSlug = parse_path(&slug)?;
    let agent = caller.agent()?.clone();
    let req = judges::validate_register(req)?;
    Ok(Json(
        state
            .write(move |cx| judges::register(cx, &agent, &slug, req))
            .await?,
    ))
}

async fn list_judges(
    State(state): State<AppState>,
    _caller: Caller,
    Path(slug): Path<String>,
) -> Result<Json<Vec<Judge>>> {
    let slug: BoardSlug = parse_path(&slug)?;
    Ok(Json(state.read(move |cx| judges::list(cx, &slug)).await?))
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

#[derive(Deserialize)]
struct SaidQuery {
    limit: Option<usize>,
}

async fn list_said(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Q(q): Q<SaidQuery>,
) -> Result<Json<Vec<Said>>> {
    caller.require_user()?;
    let id: AgentId = parse_path(&id)?;
    let limit = q.limit.unwrap_or(updates::SAID_LIST_DEFAULT);
    if !(1..=updates::SAID_KEPT).contains(&limit) {
        return Err(invalid(
            format!("limit {limit} is out of range"),
            format!("send a limit from 1 to {}", updates::SAID_KEPT),
        ));
    }
    Ok(Json(
        state
            .read(move |cx| updates::list_said(cx, &id, limit))
            .await?,
    ))
}

#[derive(Deserialize)]
struct UpdatesQuery {
    since: Option<Timestamp>,
    board: Option<BoardSlug>,
}

async fn show_updates(
    State(state): State<AppState>,
    caller: Caller,
    Q(q): Q<UpdatesQuery>,
) -> Result<Json<Updates>> {
    caller.require_user()?;
    Ok(Json(
        state
            .read(move |cx| updates::updates(cx, q.since, q.board.as_ref()))
            .await?,
    ))
}

async fn mark_seen(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<MarkSeen>,
) -> Result<Json<SeenCursor>> {
    caller.require_user()?;
    Ok(Json(
        state.write(move |cx| updates::mark_seen(cx, req)).await?,
    ))
}

async fn show_stats(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<StatsQuery>,
) -> Result<Json<Stats>> {
    let days = stats::validate_days(q.days)?;
    Ok(Json(state.read(move |cx| stats::stats(cx, days)).await?))
}

#[derive(Deserialize)]
struct SchedulerStatsQuery {
    days: Option<u32>,
    board: Option<BoardSlug>,
    policy_version: Option<String>,
}

async fn show_scheduler_stats(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<SchedulerStatsQuery>,
) -> Result<Json<SchedulerStats>> {
    let days = stats::validate_days(q.days)?;
    let policy_version = q
        .policy_version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| devices::check_token("policy_version", s, 64))
        .transpose()?;
    Ok(Json(
        state
            .read(move |cx| {
                stats::scheduler_stats(cx, days, q.board.as_ref(), policy_version.as_deref())
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct PolicyQuery {
    #[serde(default)]
    history: bool,
}

async fn show_scheduler_policy(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<PolicyQuery>,
) -> Result<Json<SchedulerPolicyResponse>> {
    Ok(Json(
        state.read(move |cx| policy::get(cx, q.history)).await?,
    ))
}

async fn put_scheduler_policy(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<PutSchedulerPolicy>,
) -> Result<Json<SchedulerPolicy>> {
    let req = policy::validate_put(req)?;
    let actor = run_actor(&caller);
    let by_user = caller.is_user();
    let caller_agent = caller.agent.clone();
    Ok(Json(
        state
            .write(move |cx| policy::put(cx, &actor, by_user, caller_agent.as_ref(), req))
            .await?,
    ))
}

/// `GET /v1/usage`: the newest account usage a device reported, or `null`.
async fn show_usage(State(state): State<AppState>, _caller: Caller) -> Result<Json<Option<Usage>>> {
    Ok(Json(state.read(usage::latest).await?))
}

/// `PUT /v1/usage`: a device daemon reports the account usage it read. Device tokens only,
/// since the reading is recorded as the calling device's.
async fn report_usage(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<ReportUsage>,
) -> Result<Json<Usage>> {
    let device = caller.device().cloned().ok_or_else(|| {
        crate::error::forbidden(
            "only a device token reports usage",
            "the board daemon reports it; run it on a device with a device token",
        )
    })?;
    Ok(Json(
        state
            .write(move |cx| {
                let req = usage::validate_report(req, cx.now)?;
                usage::report(cx, &device, req)
            })
            .await?,
    ))
}

/// `GET /v1/usage/reserve`: the weekly percent kept for the user, or `null` (pace off).
async fn show_reserve(
    State(state): State<AppState>,
    _caller: Caller,
) -> Result<Json<UsageReserve>> {
    Ok(Json(state.read(usage::reserve).await?))
}

/// `PUT /v1/usage/reserve`: anyone raises the reserve or turns it on; only the user lowers
/// it or turns it off.
async fn set_reserve(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<UsageReserve>,
) -> Result<Json<UsageReserve>> {
    let req = usage::validate_reserve(req)?;
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| usage::set_reserve(cx, req, by_user))
            .await?,
    ))
}

/// `GET /v1/usage/buckets`: lists all per-(provider, account) usage buckets.
async fn list_usage_buckets(
    State(state): State<AppState>,
    _caller: Caller,
) -> Result<Json<Vec<UsageBucket>>> {
    Ok(Json(state.read(usage::list_buckets).await?))
}

/// `PUT /v1/usage/{provider}/{account}`: a device daemon reports usage windows for one
/// `(provider, account)` bucket. Device tokens only.
async fn put_usage_bucket(
    State(state): State<AppState>,
    caller: Caller,
    Path((provider, account)): Path<(String, String)>,
    Body(req): Body<PutUsageBucket>,
) -> Result<Json<UsageBucket>> {
    let device = caller.device().cloned().ok_or_else(|| {
        crate::error::forbidden(
            "only a device token reports usage",
            "the board daemon reports it; run it on a device with a device token",
        )
    })?;
    let provider = usage::validate_bucket_key("provider", &provider)?;
    let account = usage::validate_bucket_key("account", &account)?;
    Ok(Json(
        state
            .write(move |cx| {
                let req = usage::validate_bucket_report(req, cx.now)?;
                usage::report_bucket(cx, &provider, &account, &device, req)
            })
            .await?,
    ))
}

/// `PUT /v1/usage/{provider}/{account}/reserve`: sets the weekly reserve for one bucket;
/// lowering it or turning it off is user-only.
async fn put_usage_bucket_reserve(
    State(state): State<AppState>,
    caller: Caller,
    Path((provider, account)): Path<(String, String)>,
    Body(req): Body<PutUsageBucketReserve>,
) -> Result<Json<UsageBucket>> {
    let provider = usage::validate_bucket_key("provider", &provider)?;
    let account = usage::validate_bucket_key("account", &account)?;
    let req = usage::validate_bucket_reserve(req)?;
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| usage::set_bucket_reserve(cx, &provider, &account, req, by_user))
            .await?,
    ))
}

async fn show_errors(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<feedback::ErrorsQuery>,
) -> Result<Json<ErrorLog>> {
    Ok(Json(
        state
            .read(move |cx| feedback::errors(cx, &feedback::ErrorsFilter::new(cx, q)?))
            .await?,
    ))
}

/// `POST /v1/errors`: records a refusal the calling agent's client made before sending,
/// with the server's own errors (source `client`), so signals count it.
async fn report_error(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<ClientError>,
) -> Result<StatusCode> {
    let seen = feedback::validate_client_error(req, caller.agent()?.clone())?;
    state
        .write(move |cx| feedback::record_error(cx, seen))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------
// Tasks

#[derive(Deserialize)]
struct TasksQuery {
    board: Option<BoardSlug>,
    status: Option<TaskStatus>,
    owner: Option<AgentId>,
    ready: Option<bool>,
    proposal: Option<bool>,
    mine: Option<bool>,
    parent: Option<TaskId>,
    live: Option<bool>,
    closed: Option<bool>,
    q: Option<String>,
    since: Option<String>,
    after: Option<TaskId>,
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
        proposal: q.proposal,
        parent: q.parent,
        live: q.live,
        closed: q.closed,
        words: tasks::search_words(q.q.as_deref())?,
        since: tasks::parse_since(q.since.as_deref())?,
        after: q.after,
        limit: q
            .limit
            .unwrap_or(board_proto::TASKS_LIMIT_DEFAULT as usize)
            .clamp(1, board_proto::TASKS_LIMIT_MAX as usize),
    };
    Ok(Json(state.read(move |cx| tasks::list(cx, filter)).await?))
}

async fn create_task(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateTask>,
) -> Result<Json<Task>> {
    caller.act_as(&req.agent)?;
    let req = tasks::validate_create(req, state.settings())?;
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
    let req = tasks::validate_update(req, state.settings())?;
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| tasks::update(cx, &id, req, by_user))
            .await?,
    ))
}

async fn rank_tasks(
    State(state): State<AppState>,
    caller: Caller,
    Path(slug): Path<String>,
    Body(req): Body<RankTasks>,
) -> Result<Json<RankedTasks>> {
    let slug: BoardSlug = parse_path(&slug)?;
    caller.act_as(&req.agent)?;
    tasks::validate_rank(&req)?;
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| tasks::rank(cx, &slug, req, by_user))
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
// Feedback

async fn create_feedback(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateFeedback>,
) -> Result<Json<FeedbackResult>> {
    let reporter = caller.agent()?.clone();
    let report = feedback::validate_create(req, state.settings(), reporter)?;
    Ok(Json(
        state.write(move |cx| feedback::report(cx, report)).await?,
    ))
}

async fn resolve_feedback(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<ResolveFeedback>,
) -> Result<Json<Task>> {
    let id = parse_task_id(&id)?;
    let by_user = caller.is_user();
    let agent = if by_user {
        caller.agent.clone()
    } else {
        Some(caller.agent()?.clone())
    };
    feedback::validate_resolve(&req)?;
    Ok(Json(
        state
            .write(move |cx| feedback::resolve(cx, &id, req, agent, by_user))
            .await?,
    ))
}

// ---------------------------------------------------------------------------------------
// Asks

#[derive(Deserialize)]
struct AsksQuery {
    status: Option<AskStatus>,
    board: Option<BoardSlug>,
    limit: Option<u32>,
}

async fn list_asks(
    State(state): State<AppState>,
    _caller: Caller,
    Q(q): Q<AsksQuery>,
) -> Result<Json<Vec<Ask>>> {
    Ok(Json(
        state
            .read(move |cx| {
                // A limit of 0 reads as 1, as for tasks. No upper bound: without a limit
                // the list holds every ask anyway.
                asks::list(cx, q.status, q.board.as_ref(), q.limit.map(|n| n.max(1)))
            })
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
) -> Result<Json<CreatedAsk>> {
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
    };
    let snapshot = if advance {
        // Rendered on a read connection, so snapshots of many agents render at once and
        // the writer is held only to move the cursor.
        state
            .read_then_write(move |cx| snapshot::snapshot(cx, req), snapshot::advance)
            .await?
    } else {
        state
            .read(move |cx| Ok(snapshot::snapshot(cx, req)?.snapshot))
            .await?
    };
    Ok(Json(snapshot))
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
// Devices (fleet registry & heartbeats)

async fn list_device_entries(
    State(state): State<AppState>,
    _caller: Caller,
) -> Result<Json<Vec<DeviceEntry>>> {
    Ok(Json(state.read(devices::list_entries).await?))
}

async fn device_heartbeat(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
    Body(req): Body<DeviceHeartbeat>,
) -> Result<Json<DeviceEntry>> {
    let name: DeviceName = parse_path(&name)?;
    caller.act_for_device(&name)?;
    let req = devices::validate_heartbeat(req)?;
    Ok(Json(
        state
            .write(move |cx| devices::heartbeat(cx, &name, req))
            .await?,
    ))
}

async fn list_device_wakes(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Json<Vec<StickyWake>>> {
    let name: DeviceName = parse_path(&name)?;
    caller.act_for_device(&name)?;
    require_managed_run_caller(&caller, &name)?;
    Ok(Json(
        state
            .read(move |cx| runs::list_device_wakes(cx, &name))
            .await?,
    ))
}

async fn ack_wake(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<i64>,
) -> Result<Json<StickyWake>> {
    let device = caller.device().cloned();
    let agent = caller.agent.clone();
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| runs::ack_wake(cx, id, device.as_ref(), agent.as_ref(), by_user))
            .await?,
    ))
}

// ---------------------------------------------------------------------------------------
// Dispatch (atomic pull scheduler)

async fn dispatch_claim(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<DispatchClaimRequest>,
) -> Result<Json<DispatchClaimResponse>> {
    caller.act_for_device(&req.device)?;
    require_managed_run_caller(&caller, &req.device)?;
    let req = dispatch::validate_claim(req)?;
    let actor = run_actor(&caller);
    Ok(Json(
        state
            .write(move |cx| dispatch::claim(cx, &actor, req))
            .await?,
    ))
}

// ---------------------------------------------------------------------------------------
// Runs

fn run_actor(caller: &Caller) -> String {
    if let Some(agent) = &caller.agent {
        return agent.to_string();
    }
    if !caller.is_user()
        && let Some(device) = caller.device()
    {
        return device.to_string();
    }
    "user".to_owned()
}

/// Checks that `caller` may create or mutate a `managed` run on `device`: the user, a
/// device daemon without `X-Board-Agent`, or that device's own `dispatch:<device>` agent.
/// Ordinary worker sessions (`X-Board-Agent: t3:...`, etc.) may only manage `interactive`
/// runs, so they cannot escalate themselves to managed judge/merger rights.
fn require_managed_run_caller(caller: &Caller, device: &DeviceName) -> Result<()> {
    if caller.is_user() {
        return Ok(());
    }
    if let Some(agent) = &caller.agent
        && agent.dispatch_device().as_ref() != Some(device)
    {
        return Err(forbidden(
            format!("agent {agent} cannot create or update managed runs on device {device}"),
            format!(
                "only the user or device {device}'s daemon (dispatch:{device}) may manage \
                 managed runs; interactive sessions use origin=interactive"
            ),
        ));
    }
    Ok(())
}

async fn create_run(
    State(state): State<AppState>,
    caller: Caller,
    Body(req): Body<CreateRun>,
) -> Result<Json<Run>> {
    caller.act_for_device(&req.device)?;
    let req = runs::validate_create(req)?;
    if req.origin == board_proto::RunOrigin::Managed {
        require_managed_run_caller(&caller, &req.device)?;
    }
    let actor = run_actor(&caller);
    Ok(Json(
        state.write(move |cx| runs::create(cx, &actor, req)).await?,
    ))
}

async fn list_runs(
    State(state): State<AppState>,
    _caller: Caller,
    Q(filter): Q<RunFilter>,
) -> Result<Json<Vec<Run>>> {
    let filter = runs::validate_filter(filter)?;
    Ok(Json(state.read(move |cx| runs::list(cx, filter)).await?))
}

async fn show_run(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Run>> {
    let id: RunId = parse_path(&id)?;
    Ok(Json(state.read(move |cx| runs::get(cx, &id)).await?))
}

async fn update_run(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<UpdateRun>,
) -> Result<Json<Run>> {
    let id: RunId = parse_path(&id)?;
    let req = runs::validate_update(req)?;
    let actor = run_actor(&caller);
    let device = caller.device().cloned();
    let caller_agent = caller.agent.clone();
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| {
                runs::update(
                    cx,
                    &id,
                    &actor,
                    device.as_ref(),
                    caller_agent.as_ref(),
                    by_user,
                    req,
                )
            })
            .await?,
    ))
}

async fn push_run_events(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<PushRunEvents>,
) -> Result<Json<Vec<RunEventRecord>>> {
    let id: RunId = parse_path(&id)?;
    let req = runs::validate_push_events(req)?;
    let device = caller.device().cloned();
    let caller_agent = caller.agent.clone();
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| {
                runs::push_events(
                    cx,
                    &id,
                    device.as_ref(),
                    caller_agent.as_ref(),
                    by_user,
                    req,
                )
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct RunEventsQuery {
    after_seq: Option<i64>,
    limit: Option<u32>,
}

async fn show_run_events(
    State(state): State<AppState>,
    _caller: Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
    Q(q): Q<RunEventsQuery>,
) -> Result<Response> {
    let id: RunId = parse_path(&id)?;
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
                            "Last-Event-ID is not a run event sequence number",
                            "send the seq of the last run event received, or leave it out",
                        )
                    })
            })
            .transpose()?;
        let id_check = id.clone();
        state.read(move |cx| runs::get(cx, &id_check)).await?;
        let after_seq = resume.or(q.after_seq).unwrap_or(0);
        return Ok(events::run_stream(state, id, after_seq).into_response());
    }
    let after_seq = q.after_seq.unwrap_or(0);
    let limit = q
        .limit
        .unwrap_or(RUN_EVENTS_LIMIT_DEFAULT)
        .clamp(1, RUN_EVENTS_LIMIT_MAX);
    let events = state
        .read(move |cx| {
            runs::get(cx, &id)?;
            runs::list_events(cx, &id, after_seq, limit)
        })
        .await?;
    Ok(Json(events).into_response())
}

async fn steer_run(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<SteerRunRequest>,
) -> Result<Json<SteerRunResponse>> {
    let id: RunId = parse_path(&id)?;
    let req = runs::validate_steer(req)?;
    let actor = run_actor(&caller);
    let device = caller.device().cloned();
    let caller_agent = caller.agent.clone();
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| {
                runs::steer(
                    cx,
                    &id,
                    &actor,
                    device.as_ref(),
                    caller_agent.as_ref(),
                    by_user,
                    req,
                )
            })
            .await?,
    ))
}

async fn cancel_run(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Body(req): Body<CancelRunRequest>,
) -> Result<Json<CancelRunResponse>> {
    let id: RunId = parse_path(&id)?;
    let req = runs::validate_cancel(req)?;
    let actor = run_actor(&caller);
    let device = caller.device().cloned();
    let caller_agent = caller.agent.clone();
    let by_user = caller.is_user();
    Ok(Json(
        state
            .write(move |cx| {
                runs::cancel(
                    cx,
                    &id,
                    &actor,
                    device.as_ref(),
                    caller_agent.as_ref(),
                    by_user,
                    req,
                )
            })
            .await?,
    ))
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

async fn admin_update_device(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
    Body(req): Body<AdminUpdateDevice>,
) -> Result<Json<DeviceEntry>> {
    caller.require_user()?;
    let name: DeviceName = parse_path(&name)?;
    let req = devices::validate_admin_update(req)?;
    let actor = caller.actor();
    Ok(Json(
        state
            .write(move |cx| devices::admin_update(cx, &actor, &name, req))
            .await?,
    ))
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
