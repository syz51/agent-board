//! The account usage a device daemon last read (`docs/API.md`, "Usage").

use board_proto::{DeviceName, ReportUsage, Timestamp, Usage, UsageWindow};
use rusqlite::{OptionalExtension, params};

use crate::db::{Ctx, parse_json};
use crate::error::{Result, invalid};

/// A reading dated further ahead than this is refused: the device's clock is wrong.
const CLOCK_SKEW_MINUTES: i64 = 5;

/// Checks a report before it is stored: percents are finite and in range, and the read
/// time is not ahead of the server's clock.
pub(crate) fn validate_report(req: ReportUsage, now: Timestamp) -> Result<ReportUsage> {
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
    Ok(req)
}

/// Stores the reading unless a newer one is already stored, and returns the newest.
pub(crate) fn report(cx: &mut Ctx<'_>, device: &DeviceName, req: ReportUsage) -> Result<Usage> {
    let usage = Usage {
        five_hour: req.five_hour,
        seven_day: req.seven_day,
        read_at: req.read_at,
        device: device.clone(),
    };
    cx.db.execute(
        "INSERT INTO usage (id, reading, read_at) VALUES (1, ?1, ?2) \
         ON CONFLICT (id) DO UPDATE SET reading = excluded.reading, read_at = excluded.read_at \
         WHERE excluded.read_at > usage.read_at",
        params![
            serde_json::to_string(&usage).expect("serializes"),
            usage.read_at.timestamp_millis()
        ],
    )?;
    Ok(latest(cx)?.unwrap_or(usage))
}

/// The newest reading, if any device has reported one.
pub(crate) fn latest(cx: &Ctx<'_>) -> Result<Option<Usage>> {
    Ok(cx
        .db
        .query_row("SELECT reading FROM usage WHERE id = 1", [], |r| {
            parse_json(&r.get::<_, String>(0)?)
        })
        .optional()?)
}
