//! Per-(provider, account) usage buckets and legacy `/v1/usage` alias (`docs/API.md`, "Usage").

use board_proto::{
    ApprovedUsageReserve, DeviceName, EventType, LEGACY_USAGE_ACCOUNT, LEGACY_USAGE_PROVIDER,
    PACE_PROJECT_MINUTES, PutUsageBucket, PutUsageBucketReserve, ReportUsage, Timestamp,
    USAGE_FRESH_MINUTES, Usage, UsageBucket, UsagePace, UsageReserve, UsageWindow,
};
use chrono::TimeDelta;
use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Ctx, parse_id, ts};
use crate::error::{Result, forbidden, invalid};
use crate::text::check_text;

/// Weekly readings are kept this long for the burn rate.
const HISTORY_MINUTES: i64 = 60;
/// The burn rate runs from the oldest reading between these many minutes before the newest.
const RATE_FROM_MINUTES: (i64, i64) = (30, 10);
/// A pace day; days run back from the weekly reset in these steps.
const DAY: TimeDelta = TimeDelta::hours(24);

/// A reading dated further ahead than this is refused: the device's clock is wrong.
const CLOCK_SKEW_MINUTES: i64 = 5;

/// Longest `provider` or `account` segment in a usage bucket key.
const BUCKET_KEY_MAX: usize = 64;

/// Checks a bucket key segment (`provider` or `account`): 1..=64 chars of `a-z 0-9 . _ -`,
/// starting with `a-z` or `0-9`.
pub(crate) fn validate_bucket_key(field: &str, value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > BUCKET_KEY_MAX {
        return Err(invalid(
            format!("{field} {value:?} must be 1-{BUCKET_KEY_MAX} characters"),
            format!("send {field} using 1-{BUCKET_KEY_MAX} characters of a-z, 0-9, ., _, -"),
        ));
    }
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(invalid(
            format!("{field} {value:?} must start with a lowercase letter or digit"),
            format!("send {field} starting with a-z or 0-9"),
        ));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid(
            format!("{field} {value:?} has invalid characters"),
            format!("use only a-z, 0-9, ., _ and - in {field}"),
        ));
    }
    Ok(value.to_owned())
}

/// Checks a usage report before it is stored: it has a window, percents are finite and in
/// range, and the read time is neither ahead of the server's clock nor already stale.
pub(crate) fn validate_bucket_report(
    req: PutUsageBucket,
    now: Timestamp,
) -> Result<PutUsageBucket> {
    // An empty reading, newer than the last, would release every board it holds.
    if req.five_hour.is_none() && req.seven_day.is_none() {
        return Err(invalid(
            "the reading has no five_hour or seven_day window",
            "send at least one window; if the usage endpoint gave none, report nothing",
        ));
    }
    for (field, window) in [("five_hour", &req.five_hour), ("seven_day", &req.seven_day)] {
        if let Some(UsageWindow { percent, .. }) = window
            && !(percent.is_finite() && (0.0..=1000.0).contains(percent))
        {
            return Err(invalid(
                format!("{field}.percent={percent} is out of range"),
                format!("send {field}.percent between 0 and 1000, as the usage endpoint gave it"),
            ));
        }
    }
    if req.read_at > now + TimeDelta::minutes(CLOCK_SKEW_MINUTES) {
        return Err(invalid(
            format!("read_at {} is ahead of the server's clock", req.read_at),
            "send the time the usage was read; check the device's clock",
        ));
    }
    if req.read_at < now - TimeDelta::minutes(USAGE_FRESH_MINUTES) {
        return Err(invalid(
            format!(
                "read_at {} is more than {USAGE_FRESH_MINUTES} minutes behind the server's clock, so it would hold no board",
                req.read_at
            ),
            "send a reading as soon as it is read; check the device's clock",
        ));
    }
    Ok(req)
}

/// Legacy wrapper for `PUT /v1/usage`.
pub(crate) fn validate_report(req: ReportUsage, now: Timestamp) -> Result<PutUsageBucket> {
    validate_bucket_report(req.into(), now)
}

/// Checks a reserve: finite, from 0 to below 100.
pub(crate) fn validate_bucket_reserve(req: PutUsageBucketReserve) -> Result<PutUsageBucketReserve> {
    if let Some(r) = req.reserve
        && !UsagePace::valid_reserve(r)
    {
        return Err(invalid(
            format!("reserve={r} is out of range"),
            "send reserve from 0 to below 100 (weekly percent kept for the user), or null to \
             turn the pace off",
        ));
    }
    Ok(req)
}

/// Legacy wrapper for `PUT /v1/usage/reserve`.
pub(crate) fn validate_reserve(req: UsageReserve) -> Result<PutUsageBucketReserve> {
    validate_bucket_reserve(req.into())
}

/// Checks an approved usage reserve request (`POST /v1/usage/approved`): non-empty quoted
/// words within the limit, and a valid reserve percent or `null`.
pub(crate) fn validate_approved_reserve(
    req: &ApprovedUsageReserve,
) -> Result<PutUsageBucketReserve> {
    if req.said.trim().is_empty() {
        return Err(invalid(
            "said is empty",
            "quote the user's words that approve this change",
        ));
    }
    check_text("said", &req.said, board_proto::APPROVAL_SAID_CHARS_MAX)?;
    validate_bucket_reserve(PutUsageBucketReserve {
        reserve: req.reserve,
        until: req.until,
    })
}

/// User approval recorded on `POST /v1/usage/approved`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReserveApproval<'a> {
    pub said: &'a str,
    pub device: Option<&'a DeviceName>,
}

struct StoredBucket {
    provider: String,
    account: String,
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    read_at: Option<Timestamp>,
    device: Option<DeviceName>,
    reserve: Option<f64>,
    reserve_prev: Option<f64>,
    reserve_until: Option<Timestamp>,
    anchor_day_start: Option<i64>,
    anchor_pct: Option<f64>,
    hard_since: Option<i64>,
}

const BUCKET_COLUMNS: &str = "provider, account, five_hour_pct, five_hour_resets_at, \
     seven_day_pct, seven_day_resets_at, read_at, device, reserve_pct, anchor_day_start, \
     anchor_pct, hard_since, reserve_prev_pct, reserve_until";

fn stored_bucket_row(r: &Row<'_>) -> rusqlite::Result<StoredBucket> {
    let five_hour_pct: Option<f64> = r.get(2)?;
    let five_hour_resets_at: Option<i64> = r.get(3)?;
    let seven_day_pct: Option<f64> = r.get(4)?;
    let seven_day_resets_at: Option<i64> = r.get(5)?;
    let read_at: Option<i64> = r.get(6)?;
    let device: Option<String> = r.get(7)?;
    let reserve_prev: Option<f64> = r.get(12)?;
    let reserve_until: Option<i64> = r.get(13)?;
    Ok(StoredBucket {
        provider: r.get(0)?,
        account: r.get(1)?,
        five_hour: five_hour_pct.map(|percent| UsageWindow {
            percent,
            resets_at: five_hour_resets_at.map(ts),
        }),
        seven_day: seven_day_pct.map(|percent| UsageWindow {
            percent,
            resets_at: seven_day_resets_at.map(ts),
        }),
        read_at: read_at.map(ts),
        device: device.map(|d| parse_id(&d)).transpose()?,
        reserve: r.get(8)?,
        reserve_prev: reserve_until.and(reserve_prev),
        reserve_until: reserve_until.map(ts),
        anchor_day_start: r.get(9)?,
        anchor_pct: r.get(10)?,
        hard_since: r.get(11)?,
    })
}

fn load_stored_bucket(cx: &Ctx<'_>, provider: &str, account: &str) -> Result<Option<StoredBucket>> {
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {BUCKET_COLUMNS} FROM usage_buckets WHERE provider = ?1 AND account = ?2"
        ))?
        .query_row(params![provider, account], stored_bucket_row)
        .optional()?)
}

fn to_usage_bucket(cx: &Ctx<'_>, row: StoredBucket) -> Result<UsageBucket> {
    let pace = compute_pace(cx, &row)?;
    Ok(UsageBucket {
        provider: row.provider,
        account: row.account,
        five_hour: row.five_hour,
        seven_day: row.seven_day,
        read_at: row.read_at,
        device: row.device,
        reserve: row.reserve,
        pace,
        reserve_until: row.reserve_until,
        reserve_prev: row.reserve_prev,
    })
}

/// Stores the reading for `(provider, account)` unless a newer one is already stored, and
/// returns the resulting bucket. Brand-new non-legacy buckets inherit `reserve_pct` from
/// `('anthropic', 'default')` when set so weekly pace activates on their first reading.
pub(crate) fn report_bucket(
    cx: &mut Ctx<'_>,
    provider: &str,
    account: &str,
    device: &DeviceName,
    req: PutUsageBucket,
) -> Result<UsageBucket> {
    let inherited_reserve = if provider != LEGACY_USAGE_PROVIDER || account != LEGACY_USAGE_ACCOUNT
    {
        load_stored_bucket(cx, LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT)?.and_then(|b| {
            if b.reserve_until.is_some() {
                b.reserve_prev
            } else {
                b.reserve
            }
        })
    } else {
        None
    };
    let stored = cx.db.execute(
        "INSERT INTO usage_buckets ( \
             provider, account, five_hour_pct, five_hour_resets_at, \
             seven_day_pct, seven_day_resets_at, read_at, device, reserve_pct \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT (provider, account) DO UPDATE SET \
             five_hour_pct = excluded.five_hour_pct, \
             five_hour_resets_at = excluded.five_hour_resets_at, \
             seven_day_pct = excluded.seven_day_pct, \
             seven_day_resets_at = excluded.seven_day_resets_at, \
             read_at = excluded.read_at, \
             device = excluded.device \
         WHERE usage_buckets.read_at IS NULL OR excluded.read_at > usage_buckets.read_at",
        params![
            provider,
            account,
            req.five_hour.map(|w| w.percent),
            req.five_hour
                .and_then(|w| w.resets_at)
                .map(|t| t.timestamp_millis()),
            req.seven_day.map(|w| w.percent),
            req.seven_day
                .and_then(|w| w.resets_at)
                .map(|t| t.timestamp_millis()),
            req.read_at.timestamp_millis(),
            device.as_str(),
            inherited_reserve,
        ],
    )?;
    if stored > 0
        && let Some(week) = req.seven_day
    {
        record_weekly(cx, provider, account, req.read_at, week)?;
    }
    let row = load_stored_bucket(cx, provider, account)?.expect("bucket was just upserted");
    to_usage_bucket(cx, row)
}

/// Legacy `PUT /v1/usage`: aliases `('anthropic', 'default')`.
pub(crate) fn report(cx: &mut Ctx<'_>, device: &DeviceName, req: PutUsageBucket) -> Result<Usage> {
    let bucket = report_bucket(cx, LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT, device, req)?;
    Ok(bucket.as_usage().expect("bucket has a reading"))
}

/// Keeps a newest weekly reading for the burn rate, anchors a new pace day at its first
/// reading, and marks or clears the hard line for `(provider, account)`.
fn record_weekly(
    cx: &Ctx<'_>,
    provider: &str,
    account: &str,
    at: Timestamp,
    week: UsageWindow,
) -> Result<()> {
    cx.db.execute(
        "INSERT OR REPLACE INTO usage_bucket_samples (provider, account, at, seven_day_pct) \
         VALUES (?1, ?2, ?3, ?4)",
        params![provider, account, at.timestamp_millis(), week.percent],
    )?;
    cx.db.execute(
        "DELETE FROM usage_bucket_samples WHERE provider = ?1 AND account = ?2 AND at < ?3",
        params![
            provider,
            account,
            (at - TimeDelta::minutes(HISTORY_MINUTES)).timestamp_millis()
        ],
    )?;
    if let Some(reset) = week.resets_at {
        let (start, _) = pace_day(at, reset);
        cx.db.execute(
            "UPDATE usage_buckets SET anchor_day_start = ?3, anchor_pct = ?4 \
             WHERE provider = ?1 AND account = ?2 AND anchor_day_start IS NOT ?3",
            params![provider, account, start.timestamp_millis(), week.percent],
        )?;
    }
    // Server time, as the hooks compare it with when a turn began.
    mark_hard(cx, provider, account, cx.now, week.percent)
}

/// Sets `hard_since` to `at` when the weekly use at `percent` reaches `100 - reserve` and
/// it is not set yet; clears it below the line or without a reserve.
fn mark_hard(
    cx: &Ctx<'_>,
    provider: &str,
    account: &str,
    at: Timestamp,
    percent: f64,
) -> Result<()> {
    cx.db.execute(
        "UPDATE usage_buckets SET hard_since = CASE \
           WHEN reserve_pct IS NOT NULL AND ?4 >= 100 - reserve_pct THEN COALESCE(hard_since, ?3) \
           ELSE NULL END \
         WHERE provider = ?1 AND account = ?2",
        params![provider, account, at.timestamp_millis(), percent],
    )?;
    Ok(())
}

/// The pace day that holds `at` in a week that resets at `reset`: its start, and how many
/// days run from that start to the reset, at least 1. The reset is rounded to the minute,
/// since the endpoint's reset time may move by a fraction of a second between readings.
fn pace_day(at: Timestamp, reset: Timestamp) -> (Timestamp, i32) {
    let minute = 60_000;
    let reset = ts((reset.timestamp_millis() + minute / 2).div_euclid(minute) * minute);
    let day = DAY.num_milliseconds();
    let left = (reset - at).num_milliseconds();
    // A week resets within 7 days, so this fits.
    let days = ((left + day - 1) / day).clamp(1, 30) as i32;
    (reset - DAY * days, days)
}

/// Legacy `GET /v1/usage`: newest reading of `('anthropic', 'default')`, with pace.
pub(crate) fn latest(cx: &Ctx<'_>) -> Result<Option<Usage>> {
    let Some(row) = load_stored_bucket(cx, LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT)? else {
        return Ok(None);
    };
    let bucket = to_usage_bucket(cx, row)?;
    Ok(bucket.as_usage())
}

/// Loads one usage bucket by `(provider, account)`, with computed pace, if present.
pub(crate) fn get_bucket(
    cx: &Ctx<'_>,
    provider: &str,
    account: &str,
) -> Result<Option<UsageBucket>> {
    let Some(row) = load_stored_bucket(cx, provider, account)? else {
        return Ok(None);
    };
    Ok(Some(to_usage_bucket(cx, row)?))
}

/// Lists all usage buckets (`GET /v1/usage/buckets`), ordered by `(provider, account)`.
pub(crate) fn list_buckets(cx: &Ctx<'_>) -> Result<Vec<UsageBucket>> {
    let rows: Vec<StoredBucket> = cx
        .db
        .prepare_cached(&format!(
            "SELECT {BUCKET_COLUMNS} FROM usage_buckets ORDER BY provider, account"
        ))?
        .query_map([], stored_bucket_row)?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|row| to_usage_bucket(cx, row))
        .collect()
}

/// Computes the weekly pace of `row` while a reserve is set and a weekly reading with
/// `resets_at` exists.
fn compute_pace(cx: &Ctx<'_>, row: &StoredBucket) -> Result<Option<UsagePace>> {
    let Some(reserve) = row.reserve else {
        return Ok(None);
    };
    let Some(week) = row.seven_day else {
        return Ok(None);
    };
    let Some(reset) = week.resets_at else {
        return Ok(None);
    };
    let Some(at) = row.read_at else {
        return Ok(None);
    };
    let (start, days) = pace_day(at, reset);
    // A day with no anchor yet takes this reading as its first.
    let anchor = match (row.anchor_day_start, row.anchor_pct) {
        (Some(s), Some(a)) if s == start.timestamp_millis() => a,
        _ => week.percent,
    };
    let line = anchor + (100.0 - reserve - anchor) / f64::from(days);
    let (from, to) = RATE_FROM_MINUTES;
    let earlier: Option<(i64, f64)> = cx
        .db
        .query_row(
            "SELECT at, seven_day_pct FROM usage_bucket_samples \
             WHERE provider = ?1 AND account = ?2 AND at BETWEEN ?3 AND ?4 \
             ORDER BY at LIMIT 1",
            params![
                row.provider,
                row.account,
                (at - TimeDelta::minutes(from)).timestamp_millis(),
                (at - TimeDelta::minutes(to)).timestamp_millis()
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    // Per millisecond; never negative, as across a weekly reset.
    let rate = earlier.map_or(0.0, |(then, percent)| {
        ((week.percent - percent) / (at.timestamp_millis() - then) as f64).max(0.0)
    });
    let ahead = TimeDelta::minutes(PACE_PROJECT_MINUTES).num_milliseconds() as f64;
    Ok(Some(UsagePace {
        reserve,
        day_start: start,
        anchor,
        line,
        projected: week.percent + rate * ahead,
        hard_since: row.hard_since.map(ts),
    }))
}

fn reserve_is_lower(new: Option<f64>, reference: Option<f64>) -> bool {
    match (reference, new) {
        (Some(r), Some(n)) => n < r,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Sets the weekly reserve for `(provider, account)`. Lowering it or turning it off is
/// user-only, matching `/v1/usage/reserve`. Records a `usage_reserve_updated` event when
/// the value or expiry changes, and always when `approval` is present.
pub(crate) fn set_bucket_reserve(
    cx: &mut Ctx<'_>,
    actor: &str,
    provider: &str,
    account: &str,
    req: PutUsageBucketReserve,
    by_user: bool,
    approval: Option<ReserveApproval<'_>>,
) -> Result<UsageBucket> {
    let existing = load_stored_bucket(cx, provider, account)?;
    let before = existing.as_ref().and_then(|b| b.reserve);
    let existing_prev = existing.as_ref().and_then(|b| b.reserve_prev);
    let existing_until = existing.as_ref().and_then(|b| b.reserve_until);
    // Extending an active timed reserve keeps the original baseline; a permanent update
    // clears any timer.
    let baseline = if existing_until.is_some() {
        existing_prev
    } else {
        before
    };
    let lowers_now = reserve_is_lower(req.reserve, before);
    let extends_below_baseline = reserve_is_lower(req.reserve, baseline)
        && match (req.until, existing_until) {
            (None, _) => true,
            (Some(u), Some(prev_u)) => u > prev_u,
            (Some(_), None) => true,
        };
    if (lowers_now || extends_below_baseline) && !by_user {
        let is_legacy = provider == LEGACY_USAGE_PROVIDER && account == LEGACY_USAGE_ACCOUNT;
        let next = if is_legacy {
            "a lower reserve lets agents spend more of the user's weekly usage; ask the user \
             (ask_user) to change it, or when the user already approved this change in their own \
             words, run `board approved usage --reserve <n|off> --said \"<their words>\"`"
                .to_owned()
        } else {
            format!(
                "a lower reserve lets agents spend more of the user's weekly usage; ask the user \
                 (ask_user) to change it with their user token \
                 (PUT /v1/usage/{provider}/{account}/reserve)"
            )
        };
        return Err(forbidden(
            "only the user may lower the usage reserve or turn it off",
            next,
        ));
    }
    if let Some(until) = req.until
        && until <= cx.now
    {
        return Err(invalid(
            format!("until {until} is not in the future"),
            "send a future RFC 3339 time, or leave until out for a permanent change",
        ));
    }
    let (next_prev, next_until) = match req.until {
        Some(until) if req.reserve != baseline => (baseline, Some(until)),
        _ => (None, None),
    };
    let changed =
        req.reserve != before || next_until != existing_until || next_prev != existing_prev;

    if req.reserve.is_none()
        && next_until.is_none()
        && existing.as_ref().is_some_and(|b| b.read_at.is_none())
    {
        // A bucket created solely to hold a reserve has no reading or active timer left
        // once cleared; remove the empty row so `GET /v1/usage/buckets` lists no ghost entry.
        cx.db.execute(
            "DELETE FROM usage_buckets WHERE provider = ?1 AND account = ?2 AND read_at IS NULL",
            params![provider, account],
        )?;
    } else if req.reserve.is_some() || next_until.is_some() || existing.is_some() {
        cx.db.execute(
            "INSERT INTO usage_buckets (provider, account, reserve_pct, reserve_prev_pct, reserve_until) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (provider, account) DO UPDATE SET \
                 reserve_pct = excluded.reserve_pct, \
                 reserve_prev_pct = excluded.reserve_prev_pct, \
                 reserve_until = excluded.reserve_until",
            params![
                provider,
                account,
                req.reserve,
                next_prev,
                next_until.map(|t| t.timestamp_millis()),
            ],
        )?;
        if let Some(week) = existing.as_ref().and_then(|b| b.seven_day) {
            mark_hard(cx, provider, account, cx.now, week.percent)?;
        } else if req.reserve.is_none() {
            cx.db.execute(
                "UPDATE usage_buckets SET hard_since = NULL WHERE provider = ?1 AND account = ?2",
                params![provider, account],
            )?;
        }
    }
    if changed || approval.is_some() {
        let mut payload = serde_json::json!({
            "provider": provider,
            "account": account,
            "from": before,
            "to": req.reserve,
            "by_user": by_user,
        });
        if let Some(until) = next_until {
            payload["until"] = serde_json::to_value(until).expect("timestamp serializes");
            payload["previous"] = serde_json::to_value(next_prev).expect("option serializes");
        }
        if let Some(approval) = approval {
            payload["approved"] = "said".into();
            payload["said"] = approval.said.into();
            if let Some(device) = approval.device {
                payload["device"] = device.as_str().into();
            }
        }
        cx.emit(None, actor, EventType::UsageReserveUpdated, None, payload)?;
    }
    tracing::info!(
        provider,
        account,
        reserve = ?req.reserve,
        until = ?next_until,
        by_user,
        "usage bucket reserve set"
    );
    match load_stored_bucket(cx, provider, account)? {
        Some(row) => to_usage_bucket(cx, row),
        None => Ok(UsageBucket {
            provider: provider.to_owned(),
            account: account.to_owned(),
            five_hour: None,
            seven_day: None,
            read_at: None,
            device: None,
            reserve: None,
            pace: None,
            reserve_until: None,
            reserve_prev: None,
        }),
    }
}

/// Legacy `PUT /v1/usage/reserve` and `POST /v1/usage/approved`: aliases
/// `('anthropic', 'default')`. When enabling a weekly reserve for the first time
/// (`None -> Some(reserve)`), also backfills existing non-legacy buckets whose
/// `reserve_pct` is `NULL` and not under a timed override.
pub(crate) fn set_reserve(
    cx: &mut Ctx<'_>,
    actor: &str,
    req: PutUsageBucketReserve,
    by_user: bool,
    approval: Option<ReserveApproval<'_>>,
) -> Result<UsageReserve> {
    let before_bucket = load_stored_bucket(cx, LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT)?;
    let before = before_bucket.as_ref().and_then(|b| b.reserve);
    let before_until = before_bucket.as_ref().and_then(|b| b.reserve_until);
    let new_reserve = req.reserve;
    let new_until = req.until;
    let bucket = set_bucket_reserve(
        cx,
        actor,
        LEGACY_USAGE_PROVIDER,
        LEGACY_USAGE_ACCOUNT,
        req,
        by_user,
        approval,
    )?;
    if before.is_none()
        && before_until.is_none()
        && new_until.is_none()
        && let Some(reserve) = new_reserve
    {
        let null_buckets: Vec<(String, String, Option<f64>)> = cx
            .db
            .prepare_cached(
                "SELECT provider, account, seven_day_pct FROM usage_buckets \
                 WHERE reserve_pct IS NULL \
                   AND reserve_until IS NULL \
                   AND NOT (provider = ?1 AND account = ?2)",
            )?
            .query_map(params![LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (provider, account, seven_day_pct) in null_buckets {
            cx.db.execute(
                "UPDATE usage_buckets SET reserve_pct = ?3 \
                 WHERE provider = ?1 AND account = ?2 AND reserve_pct IS NULL AND reserve_until IS NULL",
                params![provider, account, reserve],
            )?;
            if let Some(pct) = seven_day_pct {
                mark_hard(cx, &provider, &account, cx.now, pct)?;
            }
        }
    }
    Ok(UsageReserve {
        reserve: bucket.reserve,
        until: bucket.reserve_until,
        previous: bucket.reserve_prev,
    })
}

/// Legacy `GET /v1/usage/reserve`: aliases `('anthropic', 'default')`.
pub(crate) fn reserve(cx: &Ctx<'_>) -> Result<UsageReserve> {
    let row = load_stored_bucket(cx, LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT)?;
    Ok(UsageReserve {
        reserve: row.as_ref().and_then(|b| b.reserve),
        until: row.as_ref().and_then(|b| b.reserve_until),
        previous: row.as_ref().and_then(|b| b.reserve_prev),
    })
}

/// Restores timed usage reserve overrides whose `reserve_until` has passed, recording a
/// `usage_reserve_updated` event as `server` for each affected bucket.
pub(crate) fn sweep_expired_reserves(cx: &mut Ctx<'_>) -> Result<usize> {
    let now_ms = cx.now_ms();
    let due: Vec<StoredBucket> = cx
        .db
        .prepare_cached(&format!(
            "SELECT {BUCKET_COLUMNS} FROM usage_buckets \
             WHERE reserve_until IS NOT NULL AND reserve_until <= ?1 \
             ORDER BY provider, account"
        ))?
        .query_map([now_ms], stored_bucket_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut swept = 0;
    for row in due {
        let Some(until) = row.reserve_until else {
            continue;
        };
        let from = row.reserve;
        let to = row.reserve_prev;
        if to.is_none() && row.read_at.is_none() {
            cx.db.execute(
                "DELETE FROM usage_buckets WHERE provider = ?1 AND account = ?2 AND read_at IS NULL",
                params![row.provider, row.account],
            )?;
        } else {
            cx.db.execute(
                "UPDATE usage_buckets SET reserve_pct = ?3, reserve_prev_pct = NULL, \
                 reserve_until = NULL WHERE provider = ?1 AND account = ?2",
                params![row.provider, row.account, to],
            )?;
            if let Some(week) = row.seven_day {
                mark_hard(cx, &row.provider, &row.account, cx.now, week.percent)?;
            } else if to.is_none() {
                cx.db.execute(
                    "UPDATE usage_buckets SET hard_since = NULL WHERE provider = ?1 AND account = ?2",
                    params![row.provider, row.account],
                )?;
            }
        }
        cx.emit(
            None,
            "server",
            EventType::UsageReserveUpdated,
            None,
            serde_json::json!({
                "provider": row.provider,
                "account": row.account,
                "from": from,
                "to": to,
                "until": until,
                "expired": true,
            }),
        )?;
        swept += 1;
    }
    Ok(swept)
}
