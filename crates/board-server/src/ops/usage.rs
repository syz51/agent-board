//! The account usage a device daemon last read (`docs/API.md`, "Usage").

use board_proto::{
    DeviceName, PACE_PROJECT_MINUTES, ReportUsage, Timestamp, USAGE_FRESH_MINUTES, Usage,
    UsagePace, UsageReserve, UsageWindow,
};
use chrono::TimeDelta;
use rusqlite::{OptionalExtension, params};

use crate::db::{Ctx, parse_json, ts};
use crate::error::{Result, forbidden, invalid};

/// Weekly readings are kept this long for the burn rate.
const HISTORY_MINUTES: i64 = 60;
/// The burn rate runs from the oldest reading between these many minutes before the newest.
const RATE_FROM_MINUTES: (i64, i64) = (30, 10);
/// A pace day; days run back from the weekly reset in these steps.
const DAY: TimeDelta = TimeDelta::hours(24);

/// A reading dated further ahead than this is refused: the device's clock is wrong.
const CLOCK_SKEW_MINUTES: i64 = 5;

/// Checks a report before it is stored: it has a window, percents are finite and in
/// range, and the read time is neither ahead of the server's clock nor already stale.
pub(crate) fn validate_report(req: ReportUsage, now: Timestamp) -> Result<ReportUsage> {
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
    if req.read_at > now + chrono::TimeDelta::minutes(CLOCK_SKEW_MINUTES) {
        return Err(invalid(
            format!("read_at {} is ahead of the server's clock", req.read_at),
            "send the time the usage was read; check the device's clock",
        ));
    }
    if req.read_at < now - chrono::TimeDelta::minutes(USAGE_FRESH_MINUTES) {
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

/// Stores the reading unless a newer one is already stored, and returns the newest.
pub(crate) fn report(cx: &mut Ctx<'_>, device: &DeviceName, req: ReportUsage) -> Result<Usage> {
    let usage = Usage {
        five_hour: req.five_hour,
        seven_day: req.seven_day,
        read_at: req.read_at,
        device: device.clone(),
        pace: None,
    };
    let stored = cx.db.execute(
        "INSERT INTO usage (id, reading, read_at) VALUES (1, ?1, ?2) \
         ON CONFLICT (id) DO UPDATE SET reading = excluded.reading, read_at = excluded.read_at \
         WHERE excluded.read_at > usage.read_at",
        params![
            serde_json::to_string(&usage).expect("serializes"),
            usage.read_at.timestamp_millis()
        ],
    )?;
    if stored > 0
        && let Some(week) = usage.seven_day
    {
        record_weekly(cx, usage.read_at, week)?;
    }
    Ok(latest(cx)?.unwrap_or(usage))
}

/// Keeps a newest weekly reading for the burn rate, anchors a new pace day at its first
/// reading, and marks or clears the hard line.
fn record_weekly(cx: &Ctx<'_>, at: Timestamp, week: UsageWindow) -> Result<()> {
    cx.db.execute(
        "INSERT OR REPLACE INTO usage_weekly (read_at, percent) VALUES (?1, ?2)",
        params![at.timestamp_millis(), week.percent],
    )?;
    cx.db.execute(
        "DELETE FROM usage_weekly WHERE read_at < ?1",
        [(at - TimeDelta::minutes(HISTORY_MINUTES)).timestamp_millis()],
    )?;
    if let Some(reset) = week.resets_at {
        let (start, _) = pace_day(at, reset);
        cx.db.execute(
            "UPDATE usage_pace SET day_start = ?1, anchor = ?2 WHERE day_start IS NOT ?1",
            params![start.timestamp_millis(), week.percent],
        )?;
    }
    // Server time, as the hooks compare it with when a turn began.
    mark_hard(cx, cx.now, week.percent)
}

/// Sets `hard_since` to `at` when the weekly use at `percent` reaches `100 - reserve` and
/// it is not set yet; clears it below the line or without a reserve.
fn mark_hard(cx: &Ctx<'_>, at: Timestamp, percent: f64) -> Result<()> {
    cx.db.execute(
        "UPDATE usage_pace SET hard_since = CASE \
           WHEN reserve IS NOT NULL AND ?2 >= 100 - reserve THEN COALESCE(hard_since, ?1) \
           ELSE NULL END",
        params![at.timestamp_millis(), percent],
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

/// The newest reading, if any device has reported one, with the pace.
pub(crate) fn latest(cx: &Ctx<'_>) -> Result<Option<Usage>> {
    let Some(mut usage) = stored(cx)? else {
        return Ok(None);
    };
    usage.pace = pace(cx, &usage)?;
    Ok(Some(usage))
}

fn stored(cx: &Ctx<'_>) -> Result<Option<Usage>> {
    Ok(cx
        .db
        .query_row("SELECT reading FROM usage WHERE id = 1", [], |r| {
            parse_json(&r.get::<_, String>(0)?)
        })
        .optional()?)
}

/// The pace of `usage`'s weekly window while a reserve is set: the day's line from its
/// anchor, and the use projected at the burn rate of the readings 10 to 30 minutes back.
fn pace(cx: &Ctx<'_>, usage: &Usage) -> Result<Option<UsagePace>> {
    let Some(week) = usage.seven_day else {
        return Ok(None);
    };
    let Some(reset) = week.resets_at else {
        return Ok(None);
    };
    let row: (Option<f64>, Option<i64>, Option<f64>, Option<i64>) = cx.db.query_row(
        "SELECT reserve, day_start, anchor, hard_since FROM usage_pace WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let (Some(reserve), day_start, anchor, hard_since) = row else {
        return Ok(None);
    };
    let (start, days) = pace_day(usage.read_at, reset);
    // A day with no anchor yet takes this reading as its first.
    let anchor = match (day_start, anchor) {
        (Some(s), Some(a)) if s == start.timestamp_millis() => a,
        _ => week.percent,
    };
    let line = anchor + (100.0 - reserve - anchor) / f64::from(days);
    let at = usage.read_at;
    let (from, to) = RATE_FROM_MINUTES;
    let earlier: Option<(i64, f64)> = cx
        .db
        .query_row(
            "SELECT read_at, percent FROM usage_weekly WHERE read_at BETWEEN ?1 AND ?2 \
             ORDER BY read_at LIMIT 1",
            params![
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
        hard_since: hard_since.map(ts),
    }))
}

/// Checks a reserve: finite, from 0 to below 100.
pub(crate) fn validate_reserve(req: UsageReserve) -> Result<UsageReserve> {
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

/// Sets the reserve. Lowering it or turning it off is for the user only, since it lets
/// agents spend more of the week.
pub(crate) fn set_reserve(cx: &Ctx<'_>, req: UsageReserve, by_user: bool) -> Result<UsageReserve> {
    let before = reserve(cx)?.reserve;
    let lowers = match (before, req.reserve) {
        (Some(b), Some(n)) => n < b,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if lowers && !by_user {
        return Err(forbidden(
            "only the user may lower the usage reserve or turn it off",
            "a lower reserve lets agents spend more of the user's weekly usage; ask the user \
             (ask_user) to change it",
        ));
    }
    cx.db.execute(
        "UPDATE usage_pace SET reserve = ?1 WHERE id = 1",
        [req.reserve],
    )?;
    if let Some(week) = stored(cx)?.and_then(|u| u.seven_day) {
        mark_hard(cx, cx.now, week.percent)?;
    }
    tracing::info!(reserve = ?req.reserve, by_user, "usage reserve set");
    Ok(req)
}

/// The reserve now; `None` while the pace is off.
pub(crate) fn reserve(cx: &Ctx<'_>) -> Result<UsageReserve> {
    Ok(UsageReserve {
        reserve: cx
            .db
            .query_row("SELECT reserve FROM usage_pace WHERE id = 1", [], |r| {
                r.get(0)
            })?,
    })
}
