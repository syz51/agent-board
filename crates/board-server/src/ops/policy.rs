//! Versioned declarative scheduler policy (`docs/SCHEDULER_DESIGN.md`, Section 6.4 & 6.5):
//! `GET /v1/scheduler/policy` and `PUT /v1/scheduler/policy`.

use board_proto::{
    EventType, PutSchedulerPolicy, RUN_LEASE_TTL_SECS_DEFAULT, SCHEDULER_POLICY_VERSION_DEFAULT,
    STICKY_GRACE_SECS_DEFAULT, SchedulerPolicy, SchedulerPolicyResponse, SchedulerWeights,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Ctx, parse_json, ts};
use crate::error::{Result, invalid};
use crate::ops::devices::check_token;
use crate::text::check_text;

/// Maximum value any single scoring weight may take (sanity bound per Section 6.5).
const WEIGHT_MAX: f64 = 1_000.0;
/// Allowed `lease_ttl_secs` range on a declarative policy (30s to 1h).
const LEASE_TTL_MIN_SECS: u64 = 30;
const LEASE_TTL_MAX_SECS: u64 = 3_600;
/// Allowed `sticky_grace_secs` range on a declarative policy (1m to 24h).
const STICKY_GRACE_MIN_SECS: u64 = 60;
const STICKY_GRACE_MAX_SECS: u64 = 86_400;
/// Maximum characters in `PutSchedulerPolicy::notes`.
const POLICY_NOTES_CHARS_MAX: usize = 1_000;
/// Maximum historical policy versions returned by `GET /v1/scheduler/policy?history=true`.
const POLICY_HISTORY_LIMIT: i64 = 50;

/// Validated `PUT /v1/scheduler/policy` input.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedPutPolicy {
    pub version: String,
    pub weights: SchedulerWeights,
    pub lease_ttl_secs: Option<u64>,
    pub sticky_grace_secs: Option<u64>,
    pub notes: Option<String>,
}

fn check_weight(name: &str, value: f64) -> Result<f64> {
    if !value.is_finite() || !(0.0..=WEIGHT_MAX).contains(&value) {
        return Err(invalid(
            format!("weights.{name}={value} is out of range"),
            format!("send weights.{name} as a finite number between 0 and {WEIGHT_MAX}"),
        ));
    }
    Ok(value)
}

pub(crate) fn validate_weights(w: SchedulerWeights) -> Result<SchedulerWeights> {
    Ok(SchedulerWeights {
        w_kind: check_weight("w_kind", w.w_kind)?,
        w_prio: check_weight("w_prio", w.w_prio)?,
        w_rank: check_weight("w_rank", w.w_rank)?,
        w_age: check_weight("w_age", w.w_age)?,
        w_fair: check_weight("w_fair", w.w_fair)?,
        w_loc: check_weight("w_loc", w.w_loc)?,
        w_pref: check_weight("w_pref", w.w_pref)?,
        w_backfill: check_weight("w_backfill", w.w_backfill)?,
    })
}

/// Validates `PUT /v1/scheduler/policy` at the API boundary.
pub(crate) fn validate_put(req: PutSchedulerPolicy) -> Result<ValidatedPutPolicy> {
    let version = check_token("version", &req.version, 64)?;
    let weights = validate_weights(req.weights)?;
    if let Some(secs) = req.lease_ttl_secs
        && !(LEASE_TTL_MIN_SECS..=LEASE_TTL_MAX_SECS).contains(&secs)
    {
        return Err(invalid(
            format!("lease_ttl_secs={secs} is out of range"),
            format!("send lease_ttl_secs between {LEASE_TTL_MIN_SECS} and {LEASE_TTL_MAX_SECS}"),
        ));
    }
    if let Some(secs) = req.sticky_grace_secs
        && !(STICKY_GRACE_MIN_SECS..=STICKY_GRACE_MAX_SECS).contains(&secs)
    {
        return Err(invalid(
            format!("sticky_grace_secs={secs} is out of range"),
            format!(
                "send sticky_grace_secs between {STICKY_GRACE_MIN_SECS} and {STICKY_GRACE_MAX_SECS}"
            ),
        ));
    }
    let notes = req
        .notes
        .map(|raw| {
            let trimmed = raw.trim().to_owned();
            check_text("notes", &trimmed, POLICY_NOTES_CHARS_MAX)?;
            Ok::<_, crate::error::Error>(trimmed)
        })
        .transpose()?
        .filter(|s| !s.is_empty());

    Ok(ValidatedPutPolicy {
        version,
        weights,
        lease_ttl_secs: req.lease_ttl_secs,
        sticky_grace_secs: req.sticky_grace_secs,
        notes,
    })
}

fn policy_row(r: &Row<'_>) -> rusqlite::Result<SchedulerPolicy> {
    Ok(SchedulerPolicy {
        version: r.get(0)?,
        weights: parse_json(&r.get::<_, String>(1)?)?,
        lease_ttl_secs: u64::try_from(r.get::<_, i64>(2)?.max(1)).unwrap_or(180),
        sticky_grace_secs: u64::try_from(r.get::<_, i64>(3)?.max(1)).unwrap_or(600),
        notes: r.get(4)?,
        active: r.get::<_, i64>(5)? != 0,
        created_by: r.get(6)?,
        created_at: ts(r.get(7)?),
    })
}

const POLICY_COLUMNS: &str = "version, weights_json, lease_ttl_secs, sticky_grace_secs, \
     notes, active, created_by, created_at";

/// Loads the currently active `SchedulerPolicy` from `scheduler_policies`, falling back
/// to the built-in default if the table has no active row.
pub(crate) fn active(cx: &Ctx<'_>) -> Result<SchedulerPolicy> {
    let row = cx
        .db
        .prepare_cached(&format!(
            "SELECT {POLICY_COLUMNS} FROM scheduler_policies WHERE active = 1 LIMIT 1"
        ))?
        .query_row([], policy_row)
        .optional()?;
    Ok(row.unwrap_or_else(|| SchedulerPolicy {
        version: SCHEDULER_POLICY_VERSION_DEFAULT.to_owned(),
        weights: SchedulerWeights::default(),
        lease_ttl_secs: cx.s.run_lease_ttl_secs,
        sticky_grace_secs: cx.s.sticky_grace_secs,
        notes: Some("Built-in default multifactor scheduler policy".to_owned()),
        active: true,
        created_by: Some("server".to_owned()),
        created_at: ts(0),
    }))
}

/// Effective managed run lease TTL in milliseconds from `policy`, honoring explicit
/// `[defaults] run_lease_ttl_secs` server overrides while `v1-default` is active.
pub(crate) fn effective_lease_ms(cx: &Ctx<'_>, policy: &SchedulerPolicy) -> i64 {
    if policy.version == SCHEDULER_POLICY_VERSION_DEFAULT
        && cx.s.run_lease_ttl_secs != RUN_LEASE_TTL_SECS_DEFAULT
    {
        return cx.s.run_lease_ms();
    }
    i64::try_from(policy.lease_ttl_secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// Effective sticky grace window in milliseconds from the active policy, honoring explicit
/// `[defaults] sticky_grace_secs` server overrides while `v1-default` is active.
pub(crate) fn effective_sticky_grace_ms(cx: &Ctx<'_>) -> Result<i64> {
    let policy = active(cx)?;
    if policy.version == SCHEDULER_POLICY_VERSION_DEFAULT
        && cx.s.sticky_grace_secs != STICKY_GRACE_SECS_DEFAULT
    {
        return Ok(cx.s.sticky_grace_ms());
    }
    Ok(i64::try_from(policy.sticky_grace_secs.saturating_mul(1000)).unwrap_or(i64::MAX))
}

/// `GET /v1/scheduler/policy?history=true|false`.
pub(crate) fn get(cx: &Ctx<'_>, include_history: bool) -> Result<SchedulerPolicyResponse> {
    let policy = active(cx)?;
    let history = if include_history {
        cx.db
            .prepare_cached(&format!(
                "SELECT {POLICY_COLUMNS} FROM scheduler_policies \
                 ORDER BY created_at DESC, version DESC LIMIT ?1"
            ))?
            .query_map([POLICY_HISTORY_LIMIT], policy_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok(SchedulerPolicyResponse { policy, history })
}

/// `PUT /v1/scheduler/policy`: inserts and activates a new immutable versioned policy
/// snapshot and emits a `scheduler_policy_updated` Trail Rule event.
pub(crate) fn put(
    cx: &mut Ctx<'_>,
    actor: &str,
    req: ValidatedPutPolicy,
) -> Result<SchedulerPolicy> {
    if cx.exists(
        "SELECT 1 FROM scheduler_policies WHERE version = ?1",
        [req.version.as_str()],
    )? {
        return Err(invalid(
            format!("scheduler policy version {:?} already exists", req.version),
            "choose a new version name (e.g. v2-fair-share); existing policy versions are immutable",
        ));
    }
    let prev = active(cx)?;
    let lease_ttl_secs = req.lease_ttl_secs.unwrap_or(prev.lease_ttl_secs);
    let sticky_grace_secs = req.sticky_grace_secs.unwrap_or(prev.sticky_grace_secs);
    let weights_json = serde_json::to_string(&req.weights).expect("weights serialize");
    let now = cx.now_ms();

    cx.db.execute(
        "UPDATE scheduler_policies SET active = 0 WHERE active = 1",
        [],
    )?;
    cx.db.execute(
        "INSERT INTO scheduler_policies ( \
             version, weights_json, lease_ttl_secs, sticky_grace_secs, \
             notes, active, created_by, created_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
        params![
            req.version,
            weights_json,
            i64::try_from(lease_ttl_secs).unwrap_or(180),
            i64::try_from(sticky_grace_secs).unwrap_or(600),
            req.notes,
            actor,
            now,
        ],
    )?;

    let mut payload = serde_json::json!({
        "version": req.version,
        "previous_version": prev.version,
        "weights": req.weights,
        "lease_ttl_secs": lease_ttl_secs,
        "sticky_grace_secs": sticky_grace_secs,
    });
    if let Some(ref notes) = req.notes {
        payload["notes"] = serde_json::json!(notes);
    }
    cx.emit(
        None,
        actor,
        EventType::SchedulerPolicyUpdated,
        None,
        payload,
    )?;

    active(cx)
}
