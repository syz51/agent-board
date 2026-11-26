//! Host health telemetry, cross-machine scheduled jobs, token/cache analytics,
//! and aggregated `/v1/health/overview` computation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use board_proto::{
    AlertSeverity, BackupState, Board, BoardSlug, BoardStatus, DailyTokenBucket, DeployTrainStatus,
    DeviceEntry, DeviceName, DeviceRole, DeviceStatus, EventType, ExpiryItem, FactoryHealthSummary,
    GoalCooldownStatus, HOST_HEALTH_SAMPLES_RETENTION_DAYS, HOST_HEALTH_TTL_SECS_MAX,
    HOST_HEALTH_TTL_SECS_MIN, HealthOverview, HealthPosture, HostBackupStatus, HostCiHealth,
    HostExpiryItem, HostExternalAlert, HostHealthAck, HostHealthReport, HostHealthSummary,
    HostResourceSample, HostSchedulesAck, HostStorageQuota, HostVaultHealth, HostVersionEntry,
    IncidentCounts, LeaseHygieneItem, LeaseHygieneSummary, PostHostSchedules, QueueCounts,
    SCHEDULE_BATCH_MAX, SCHEDULE_COMMAND_CHARS_MAX, SCHEDULE_DEEP_LINK_CHARS_MAX,
    SCHEDULE_ERROR_BYTES_MAX, SCHEDULE_HISTORY_CAP, ScheduleCategory, ScheduleHistoryResponse,
    ScheduleListResponse, ScheduleSource, ScheduleState, ScheduleSummary, ScheduledJobHistoryEntry,
    ScheduledJobSnapshot, ServerHealthSummary, ServiceState, TaskId, Timestamp,
    TokenAnalyticsSummary, TokenBreakdownRow, VersionSkew,
};
use chrono::TimeDelta;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ts};
use crate::error::{Result, invalid, not_found};
use crate::ops::devices::{check_token, list_entries as list_device_entries};
use crate::ops::runs::{after_merge_ready_at, has_after_merge_checks, standing_review_pass};
use crate::ops::usage::list_buckets as list_usage_buckets;
use crate::store::{list_boards, load_tasks};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
/// Maximum allowed clock skew ahead of server time for reported timestamps.
const CLOCK_SKEW_SECS: i64 = 300;
/// Maximum containers, services, storage, backups, certs, or alerts per host report.
const REPORT_LIST_MAX: usize = 256;
/// Maximum version entries per host report.
const REPORT_VERSIONS_MAX: usize = 64;
/// Maximum recent hourly samples returned per host in `GET /v1/health/overview`.
const SAMPLES_PER_HOST_MAX: i64 = 48;

/// Validation for `POST /v1/hosts/{host}/health`.
pub(crate) fn validate_health_report(mut req: HostHealthReport) -> Result<HostHealthReport> {
    req.collector = check_token("collector", &req.collector, 40)?;
    if !(HOST_HEALTH_TTL_SECS_MIN..=HOST_HEALTH_TTL_SECS_MAX).contains(&req.ttl_secs) {
        return Err(invalid(
            format!("ttl_secs={} is out of range", req.ttl_secs),
            format!(
                "send ttl_secs between {HOST_HEALTH_TTL_SECS_MIN} and {HOST_HEALTH_TTL_SECS_MAX}"
            ),
        ));
    }
    if req.versions.len() > REPORT_VERSIONS_MAX {
        return Err(invalid(
            format!(
                "versions has {} entries; the limit is {REPORT_VERSIONS_MAX}",
                req.versions.len()
            ),
            format!("send at most {REPORT_VERSIONS_MAX} version entries"),
        ));
    }
    for (label, len) in [
        ("containers", req.containers.len()),
        ("services", req.services.len()),
        ("storage", req.storage.len()),
        ("backups", req.backups.len()),
        ("certs", req.certs.len()),
        ("alerts", req.alerts.len()),
    ] {
        if len > REPORT_LIST_MAX {
            return Err(invalid(
                format!("{label} has {len} entries; the limit is {REPORT_LIST_MAX}"),
                format!("send at most {REPORT_LIST_MAX} {label} entries"),
            ));
        }
    }
    if let Some(res) = &req.resources {
        for (name, val) in [
            ("cpu_pct", res.cpu_pct),
            ("load_1m", res.load_1m),
            ("load_5m", res.load_5m),
            ("load_15m", res.load_15m),
            ("mem_used_gib", res.mem_used_gib),
            ("mem_total_gib", res.mem_total_gib),
            ("disk_used_gib", res.disk_used_gib),
            ("disk_total_gib", res.disk_total_gib),
            ("disk_free_gib", res.disk_free_gib),
            ("disk_days_to_full", res.disk_days_to_full),
            ("slice_cpu_cap_pct", res.slice_cpu_cap_pct),
        ] {
            if let Some(v) = val
                && (!v.is_finite() || v < 0.0)
            {
                return Err(invalid(
                    format!("resources.{name}={v} must be finite and non-negative"),
                    format!("send a non-negative finite number for resources.{name}"),
                ));
            }
        }
    }
    for s in &req.storage {
        if !s.used_gib.is_finite()
            || !s.quota_gib.is_finite()
            || s.used_gib < 0.0
            || s.quota_gib < 0.0
        {
            return Err(invalid(
                format!("storage {:?} has invalid quota numbers", s.name),
                "send finite non-negative used_gib and quota_gib values",
            ));
        }
    }
    Ok(req)
}

/// Validation for `POST /v1/hosts/{host}/schedules`.
pub(crate) fn validate_schedules_report(mut req: PostHostSchedules) -> Result<PostHostSchedules> {
    if req.source == ScheduleSource::Unknown {
        return Err(invalid(
            "unknown schedule source",
            "send source as launchd, systemd, t3, cron, github, board, or other",
        ));
    }
    if req.jobs.len() > SCHEDULE_BATCH_MAX {
        return Err(invalid(
            format!(
                "jobs has {} entries; the limit is {SCHEDULE_BATCH_MAX}",
                req.jobs.len()
            ),
            format!("send at most {SCHEDULE_BATCH_MAX} scheduled jobs per source"),
        ));
    }
    let mut seen_ids = BTreeSet::new();
    for job in &mut req.jobs {
        let id = job.id.trim().to_owned();
        if id.is_empty() || id.chars().count() > 160 || id.chars().any(char::is_control) {
            return Err(invalid(
                format!("job id {:?} is invalid", job.id),
                "send a non-empty job id of at most 160 printable characters",
            ));
        }
        if !seen_ids.insert(id.clone()) {
            return Err(invalid(
                format!("duplicate job id {id:?} in batch"),
                "send each job id once per (host, source) batch",
            ));
        }
        job.id = id;
        let name = job.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(invalid(
                format!("job {:?} name is empty or exceeds 200 characters", job.id),
                "send a non-empty name up to 200 characters",
            ));
        }
        job.name = name;
        if job.category == ScheduleCategory::Unknown {
            return Err(invalid(
                format!("job {:?} has unknown category", job.id),
                "send a valid category such as backup, monitoring, agent_ops, sync, security, maintenance, research, app, os, or other",
            ));
        }
        if job.state == ScheduleState::Unknown {
            return Err(invalid(
                format!("job {:?} has unknown state", job.id),
                "send state as ok, running, failing, overdue, never_run, or disabled",
            ));
        }
        if job.schedule_human.trim().is_empty() || job.schedule_human.chars().count() > 120 {
            return Err(invalid(
                format!(
                    "job {:?} schedule_human is empty or exceeds 120 characters",
                    job.id
                ),
                "send a concise human cadence up to 120 characters",
            ));
        }
        if job.schedule_raw.chars().count() > 500 {
            return Err(invalid(
                format!("job {:?} schedule_raw exceeds 500 characters", job.id),
                "shorten schedule_raw to at most 500 characters",
            ));
        }
        if let Some(err) = &mut job.last_error
            && err.len() > SCHEDULE_ERROR_BYTES_MAX
        {
            *err = truncate_utf8_bytes(err, SCHEDULE_ERROR_BYTES_MAX);
        }
        if let Some(cmd) = &mut job.command_preview
            && cmd.chars().count() > SCHEDULE_COMMAND_CHARS_MAX
        {
            *cmd = cmd.chars().take(SCHEDULE_COMMAND_CHARS_MAX).collect();
        }
        if let Some(link) = &job.deep_link {
            let trimmed = link.trim();
            if trimmed.is_empty() {
                job.deep_link = None;
            } else if trimmed.chars().count() > SCHEDULE_DEEP_LINK_CHARS_MAX
                || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
                || !(trimmed.starts_with("https://")
                    || trimmed.starts_with("t3://")
                    || (trimmed.starts_with('/') && !trimmed.starts_with("//")))
            {
                return Err(invalid(
                    format!("job {:?} deep_link {trimmed:?} is invalid", job.id),
                    "send deep_link starting with https://, t3://, or / without whitespace",
                ));
            } else {
                job.deep_link = Some(trimmed.to_owned());
            }
        }
    }
    Ok(req)
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn clamp_reported_at(reported_at: Option<Timestamp>, now: Timestamp) -> Result<Timestamp> {
    match reported_at {
        Some(at) => {
            if at > now + TimeDelta::seconds(CLOCK_SKEW_SECS) {
                return Err(invalid(
                    format!("reported_at {at} is ahead of the server's clock"),
                    "send the time collected on the host and check NTP synchronization",
                ));
            }
            Ok(at)
        }
        None => Ok(now),
    }
}

/// Computes a deterministic hash of state-significant fields in a health report so steady-state
/// CPU/RAM/disk ticks do not emit noisy `host_health_updated` events, while alert/service/
/// container/backup/version/cert changes emit immediately over SSE.
fn health_summary_hash(report: &HostHealthReport) -> String {
    let mut hasher = Sha256::new();
    let digest_obj = serde_json::json!({
        "versions": report.versions,
        "containers": report.containers.iter().map(|c| (&c.name, &c.state, &c.health, c.restarts)).collect::<Vec<_>>(),
        "services": report.services.iter().map(|s| (&s.name, s.state, s.restarts)).collect::<Vec<_>>(),
        "backups": report.backups.iter().map(|b| (&b.target, b.state)).collect::<Vec<_>>(),
        "certs": report.certs.iter().map(|c| (&c.name, c.ok, c.expires_at)).collect::<Vec<_>>(),
        "alerts": report.alerts.iter().map(|a| (&a.name, a.severity, a.is_watchdog, &a.host)).collect::<Vec<_>>(),
        "vault": report.vault.as_ref().map(|v| (v.reachable, v.sealed, v.audit_ok)),
        "ci": report.ci.as_ref().map(|c| (c.poller_fresh, c.egress_ok, c.webhook_errors)),
        "network": report.network.as_ref().map(|n| (n.tailscale_online, n.heartbeat_ok)),
    });
    hasher.update(digest_obj.to_string().as_bytes());
    crate::auth::hex(&hasher.finalize())
}

/// `POST /v1/hosts/{host}/health`: stores the latest snapshot for `(host, collector)`, updates
/// the hourly `host_health_samples` rollup, prunes samples older than 30 days, and emits a
/// `host_health_updated` event when state-significant fields changed.
pub(crate) fn upsert_host_health(
    cx: &mut Ctx<'_>,
    actor: &str,
    host: &DeviceName,
    mut report: HostHealthReport,
) -> Result<HostHealthAck> {
    let reported_at = clamp_reported_at(report.reported_at, cx.now)?;
    report.reported_at = Some(reported_at);
    let reported_ms = reported_at.timestamp_millis();
    let now_ms = cx.now_ms();
    let collector = report.collector.clone();
    let ttl_secs = report.ttl_secs;
    let summary_hash = health_summary_hash(&report);
    let report_json = serde_json::to_string(&report).expect("HostHealthReport serializes");

    let prev_hash: Option<String> = cx
        .db
        .query_row(
            "SELECT summary_hash FROM host_health_snapshots WHERE host = ?1 AND collector = ?2",
            params![host.as_str(), &collector],
            |r| r.get(0),
        )
        .optional()?;

    cx.db.execute(
        "INSERT INTO host_health_snapshots \
         (host, collector, report_json, summary_hash, reported_at, ttl_secs, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (host, collector) DO UPDATE SET \
             report_json = excluded.report_json, \
             summary_hash = excluded.summary_hash, \
             reported_at = excluded.reported_at, \
             ttl_secs = excluded.ttl_secs, \
             updated_at = excluded.updated_at",
        params![
            host.as_str(),
            &collector,
            &report_json,
            &summary_hash,
            reported_ms,
            ttl_secs as i64,
            now_ms,
        ],
    )?;

    let mut recorded_sample = false;
    if let Some(res) = &report.resources {
        let mem_pct = match (res.mem_used_gib, res.mem_total_gib) {
            (Some(u), Some(t)) if t > 0.0 => Some(((u / t) * 100.0).clamp(0.0, 100.0)),
            _ => None,
        };
        let disk_pct = match (res.disk_used_gib, res.disk_total_gib) {
            (Some(u), Some(t)) if t > 0.0 => Some(((u / t) * 100.0).clamp(0.0, 100.0)),
            _ => None,
        };
        if res.cpu_pct.is_some()
            || mem_pct.is_some()
            || disk_pct.is_some()
            || res.disk_free_gib.is_some()
        {
            let hour_bucket = reported_ms.div_euclid(HOUR_MS) * HOUR_MS;
            cx.db.execute(
                "INSERT INTO host_health_samples \
                 (host, hour_bucket, cpu_pct, mem_used_pct, disk_used_pct, disk_free_gib, samples, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7) \
                 ON CONFLICT (host, hour_bucket) DO UPDATE SET \
                     cpu_pct = COALESCE(excluded.cpu_pct, host_health_samples.cpu_pct), \
                     mem_used_pct = COALESCE(excluded.mem_used_pct, host_health_samples.mem_used_pct), \
                     disk_used_pct = COALESCE(excluded.disk_used_pct, host_health_samples.disk_used_pct), \
                     disk_free_gib = COALESCE(excluded.disk_free_gib, host_health_samples.disk_free_gib), \
                     samples = host_health_samples.samples + 1, \
                     recorded_at = excluded.recorded_at",
                params![
                    host.as_str(),
                    hour_bucket,
                    res.cpu_pct,
                    mem_pct,
                    disk_pct,
                    res.disk_free_gib,
                    now_ms,
                ],
            )?;
            recorded_sample = true;
        }
    }

    // Keep hourly samples strictly bounded to 30 days.
    prune_samples(cx)?;

    if prev_hash.as_deref() != Some(summary_hash.as_str()) {
        cx.emit(
            None,
            actor,
            EventType::HostHealthUpdated,
            None,
            serde_json::json!({
                "host": host,
                "collector": collector,
            }),
        )?;
    }

    Ok(HostHealthAck {
        host: host.clone(),
        collector,
        reported_at,
        ttl_secs,
        recorded_sample,
    })
}

/// Deletes `host_health_samples` older than 30 days (called on sample write and in the minute sweeper).
pub(crate) fn prune_samples(cx: &Ctx<'_>) -> Result<usize> {
    let cutoff = cx
        .now_ms()
        .saturating_sub(HOST_HEALTH_SAMPLES_RETENTION_DAYS * DAY_MS);
    Ok(cx.db.execute(
        "DELETE FROM host_health_samples WHERE recorded_at < ?1",
        [cutoff],
    )?)
}

struct PrevScheduledJob {
    state: String,
    enabled: bool,
    schedule_raw: String,
    last_run_at: Option<i64>,
    next_run_at: Option<i64>,
    expected_interval_secs: Option<i64>,
    last_exit_code: Option<i32>,
    run_count: Option<i64>,
    run_count_observed_at: Option<i64>,
    last_error: Option<String>,
    reported_at: i64,
}

struct ResolvedScheduleTimes {
    last_run_ms: Option<i64>,
    next_run_ms: Option<i64>,
    run_count_observed_at: Option<i64>,
    run_observed: bool,
    incoming_explains_run: bool,
}

fn earlier_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Resolves effective `(last_run_at, next_run_at, run_count_observed_at)` for a reported job.
///
/// For `launchd`, `launchctl print` exposes a monotonic `runs` counter and `last exit code`, but
/// no last-run timestamp, so host collectors infer `last_run_at` from stdout/stderr log mtime.
/// Clean silent jobs (`last_exit_code == 0`) increment `runs` without updating log mtime. This
/// tracks `run_count` deltas (and resets after reboot/reload) so silent runs advance `last_run_at`,
/// `next_run_at`, and history timestamps, while still flagging jobs `overdue` if `runs` stops
/// advancing for longer than `expected_interval_secs + grace`.
///
/// Non-`launchd` sources (`systemd`, `t3`, etc.) provide authoritative timestamps and pass
/// through unchanged.
#[allow(clippy::too_many_arguments)]
fn resolve_schedule_timestamps(
    source: ScheduleSource,
    state: ScheduleState,
    enabled: bool,
    schedule_raw: &str,
    incoming_last_run_ms: Option<i64>,
    incoming_next_run_ms: Option<i64>,
    expected_secs: Option<i64>,
    last_exit_code: Option<i32>,
    run_count: Option<i64>,
    reported_ms: i64,
    prev: Option<&PrevScheduledJob>,
) -> ResolvedScheduleTimes {
    if source != ScheduleSource::Launchd {
        let count_increased = match (prev.and_then(|p| p.run_count), run_count) {
            (Some(old), Some(new)) => new > old,
            (None, Some(new)) => new > 0,
            _ => false,
        };
        let incoming_explains_run = match (prev.and_then(|p| p.last_run_at), incoming_last_run_ms) {
            (Some(old), Some(new)) => new > old,
            (None, Some(_)) => true,
            _ => false,
        };
        return ResolvedScheduleTimes {
            last_run_ms: incoming_last_run_ms,
            next_run_ms: incoming_next_run_ms,
            run_count_observed_at: None,
            run_observed: count_increased,
            incoming_explains_run,
        };
    }

    let Some(new_rc) = run_count else {
        let incoming_explains_run = match (prev.and_then(|p| p.last_run_at), incoming_last_run_ms) {
            (Some(old), Some(new)) => new > old,
            (None, Some(_)) => true,
            _ => false,
        };
        return ResolvedScheduleTimes {
            last_run_ms: incoming_last_run_ms,
            next_run_ms: incoming_next_run_ms,
            run_count_observed_at: None,
            run_observed: false,
            incoming_explains_run,
        };
    };

    // Disabled launchd jobs have no upcoming run: never fabricate or retain a next_run_at, and
    // clear `run_count_observed_at` so re-enabling later starts a fresh observation window rather
    // than immediately flagging overdue against a pre-disable deadline.
    if !enabled {
        let last_run_ms = match (prev.and_then(|p| p.last_run_at), incoming_last_run_ms) {
            (Some(old), Some(inc)) => Some(old.max(inc)),
            (Some(old), None) => Some(old),
            (None, Some(inc)) => Some(inc),
            (None, None) => None,
        };
        return ResolvedScheduleTimes {
            last_run_ms,
            next_run_ms: None,
            run_count_observed_at: None,
            run_observed: false,
            incoming_explains_run: false,
        };
    }

    let interval_ms = expected_secs
        .filter(|&s| s > 0)
        .map(|s| s.saturating_mul(1_000));

    // A job that has never run (or was reloaded back to runs=0).
    if new_rc == 0 {
        let observed = prev
            .filter(|p| p.enabled && p.run_count == Some(0))
            .and_then(|p| p.run_count_observed_at)
            .unwrap_or(reported_ms);
        return ResolvedScheduleTimes {
            last_run_ms: incoming_last_run_ms,
            next_run_ms: incoming_next_run_ms,
            run_count_observed_at: Some(observed),
            run_observed: false,
            incoming_explains_run: false,
        };
    }

    const MTIME_SLOP_MS: i64 = 5_000;

    let Some(p) = prev else {
        // First time the server sees this launchd job. If it is healthy (ok + exit 0) with a
        // StartInterval cadence and missing/stale next_run_at (or a far-future calendar slot on
        // a combined StartInterval + StartCalendarInterval job), bound next_run_at by a single
        // interval observation window instead of falsely marking it overdue immediately.
        let healthy_ok = state == ScheduleState::Ok && last_exit_code == Some(0);
        let next_run_ms = if healthy_ok {
            let incoming_future = incoming_next_run_ms.filter(|&n| n > reported_ms);
            let interval_next = interval_ms.map(|iv| reported_ms.saturating_add(iv));
            earlier_opt(incoming_future, interval_next).or(incoming_next_run_ms)
        } else {
            incoming_next_run_ms
        };
        return ResolvedScheduleTimes {
            last_run_ms: incoming_last_run_ms,
            next_run_ms,
            run_count_observed_at: Some(reported_ms),
            run_observed: false,
            incoming_explains_run: incoming_last_run_ms.is_some(),
        };
    };

    let count_increased = match p.run_count {
        Some(old) => new_rc > old,
        None => new_rc > 0,
    };
    // Reboot or launchctl bootout+bootstrap resets `runs` to a smaller positive number once it
    // executes again.
    let count_reset_with_run = matches!(p.run_count, Some(old) if new_rc < old && new_rc > 0);
    let runs_delta = match p.run_count {
        Some(old) if new_rc > old => new_rc - old,
        Some(old) if new_rc < old && new_rc > 0 => new_rc,
        None if new_rc > 0 => new_rc,
        _ => 0,
    };
    let incoming_fresh_mtime = incoming_last_run_ms.is_some_and(|m| {
        let newer_than_prev = m >= p.reported_at.saturating_sub(MTIME_SLOP_MS)
            && p.last_run_at.is_none_or(|old| m > old);
        // If multiple runs occurred since the last poll and `m + interval <= reported_ms`, at
        // least one later run in that burst was silent, so `m` is not the latest run's timestamp.
        let stale_after_multi_run =
            runs_delta > 1 && interval_ms.is_some_and(|iv| m.saturating_add(iv) <= reported_ms);
        newer_than_prev && !stale_after_multi_run
    });

    if count_increased || count_reset_with_run {
        let last_run_ms = if incoming_fresh_mtime {
            incoming_last_run_ms
        } else {
            Some(reported_ms)
        };
        let next_run_ms = if incoming_fresh_mtime {
            let incoming_after_last =
                incoming_next_run_ms.filter(|&n| last_run_ms.is_none_or(|l| n > l));
            let interval_next = last_run_ms
                .zip(interval_ms)
                .map(|(l, iv)| l.saturating_add(iv));
            earlier_opt(incoming_after_last, interval_next).or(incoming_next_run_ms)
        } else {
            let incoming_future = incoming_next_run_ms.filter(|&n| n > reported_ms);
            let interval_next = interval_ms.map(|iv| reported_ms.saturating_add(iv));
            earlier_opt(incoming_future, interval_next).or(incoming_next_run_ms)
        };
        return ResolvedScheduleTimes {
            last_run_ms,
            next_run_ms,
            run_count_observed_at: Some(reported_ms),
            run_observed: true,
            incoming_explains_run: incoming_fresh_mtime,
        };
    }

    // `runs` is unchanged since the previous report.
    if incoming_fresh_mtime {
        // Log output arrived for a run whose `runs` counter already incremented on an earlier poll
        // (e.g. a long-running job that printed after starting).
        let last_run_ms = incoming_last_run_ms;
        let incoming_after_last =
            incoming_next_run_ms.filter(|&n| last_run_ms.is_none_or(|l| n > l));
        let interval_next = last_run_ms
            .zip(interval_ms)
            .map(|(l, iv)| l.saturating_add(iv));
        let next_run_ms = earlier_opt(incoming_after_last, interval_next).or(incoming_next_run_ms);
        return ResolvedScheduleTimes {
            last_run_ms,
            next_run_ms,
            run_count_observed_at: Some(reported_ms),
            run_observed: false,
            incoming_explains_run: true,
        };
    }

    // Preserve the server's newer observed `last_run_at` so stale stdout/stderr mtime never
    // moves it backwards.
    let last_run_ms = match (p.last_run_at, incoming_last_run_ms) {
        (Some(old), Some(inc)) => Some(old.max(inc)),
        (Some(old), None) => Some(old),
        (None, Some(inc)) => Some(inc),
        (None, None) => None,
    };

    // If `run_count_observed_at` is NULL (pre-migration 0039 row) or the job was just re-enabled,
    // initialize a single observation baseline at `reported_ms` once. Subsequent polls see
    // `Some(obs)` while enabled and never slide the deadline forward.
    let Some(obs) = p.run_count_observed_at.filter(|_| p.enabled) else {
        let healthy_ok = state == ScheduleState::Ok && last_exit_code == Some(0);
        if healthy_ok {
            let incoming_future = incoming_next_run_ms.filter(|&n| n > reported_ms);
            let interval_next = interval_ms.map(|iv| reported_ms.saturating_add(iv));
            let next_run_ms = earlier_opt(incoming_future, interval_next).or(incoming_next_run_ms);
            return ResolvedScheduleTimes {
                last_run_ms,
                next_run_ms,
                run_count_observed_at: Some(reported_ms),
                run_observed: false,
                incoming_explains_run: false,
            };
        }
        return ResolvedScheduleTimes {
            last_run_ms,
            next_run_ms: incoming_next_run_ms.or(p.next_run_at),
            run_count_observed_at: Some(p.last_run_at.unwrap_or(p.reported_at)),
            run_observed: false,
            incoming_explains_run: false,
        };
    };

    // Keep the pinned due deadline from when `runs` was last observed so a genuinely stalled job
    // (including combined StartInterval + StartCalendarInterval jobs whose past interval candidate
    // was dropped by the collector) cannot slide its deadline forward to a far-future calendar
    // slot while `runs` stays stuck.
    let schedule_unchanged =
        p.expected_interval_secs == expected_secs && p.schedule_raw == schedule_raw;
    let next_run_ms = if schedule_unchanged && let Some(prev_next) = p.next_run_at {
        match incoming_next_run_ms {
            Some(n) if n > reported_ms && n < prev_next => Some(n),
            _ => Some(prev_next),
        }
    } else {
        let incoming_future = incoming_next_run_ms.filter(|&n| n > reported_ms);
        let interval_next = interval_ms.map(|iv| {
            let anchor = last_run_ms
                .filter(|&l| l >= obs.saturating_sub(iv))
                .unwrap_or(obs);
            anchor.saturating_add(iv)
        });
        earlier_opt(incoming_future, interval_next).or(incoming_next_run_ms)
    };

    ResolvedScheduleTimes {
        last_run_ms,
        next_run_ms,
        run_count_observed_at: Some(obs),
        run_observed: false,
        incoming_explains_run: false,
    }
}

/// `POST /v1/hosts/{host}/schedules`: upserts the full job list for `(host, source)`, diffs
/// against existing rows to record ring-buffered run-history transitions (capped at 30/job),
/// prunes removed jobs, and emits `schedules_updated` when anything changed.
pub(crate) fn upsert_host_schedules(
    cx: &mut Ctx<'_>,
    actor: &str,
    host: &DeviceName,
    req: PostHostSchedules,
) -> Result<HostSchedulesAck> {
    let reported_at = clamp_reported_at(req.reported_at, cx.now)?;
    let reported_ms = reported_at.timestamp_millis();
    let now_ms = cx.now_ms();
    let source_str = req.source.as_str();

    let mut existing: HashMap<String, PrevScheduledJob> = cx
        .db
        .prepare_cached(
            "SELECT job_key, state, enabled, schedule_raw, last_run_at, next_run_at, \
                    expected_interval_secs, last_exit_code, run_count, run_count_observed_at, \
                    last_error, reported_at \
             FROM scheduled_jobs WHERE host = ?1 AND source = ?2",
        )?
        .query_map(params![host.as_str(), source_str], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PrevScheduledJob {
                    state: r.get(1)?,
                    enabled: r.get::<_, i64>(2)? != 0,
                    schedule_raw: r.get(3)?,
                    last_run_at: r.get(4)?,
                    next_run_at: r.get(5)?,
                    expected_interval_secs: r.get(6)?,
                    last_exit_code: r.get(7)?,
                    run_count: r.get(8)?,
                    run_count_observed_at: r.get(9)?,
                    last_error: r.get(10)?,
                    reported_at: r.get(11)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut incoming_keys: BTreeSet<String> = BTreeSet::new();
    let mut history_recorded: u32 = 0;
    let mut changed = false;

    for job in &req.jobs {
        let job_key = format!("{}:{}:{}", host.as_str(), source_str, job.id);
        incoming_keys.insert(job_key.clone());

        let state_str = enum_str(&job.state);
        let category_str = enum_str(&job.category);
        let incoming_last_run_ms = job.last_run_at.map(|t| t.timestamp_millis());
        let incoming_next_run_ms = job.next_run_at.map(|t| t.timestamp_millis());
        let duration_ms = job.last_duration_ms.map(|v| v as i64);
        let run_count = job.run_count.map(|v| v as i64);
        let expected_secs = job.expected_interval_secs.map(|v| v as i64);

        let prev = existing.remove(&job_key);
        let resolved = resolve_schedule_timestamps(
            req.source,
            job.state,
            job.enabled,
            &job.schedule_raw,
            incoming_last_run_ms,
            incoming_next_run_ms,
            expected_secs,
            job.last_exit_code,
            run_count,
            reported_ms,
            prev.as_ref(),
        );
        let last_run_ms = resolved.last_run_ms;
        let next_run_ms = resolved.next_run_ms;

        let (should_record_history, state_changed, job_changed) = match &prev {
            None => (
                last_run_ms.is_some()
                    || job.state == ScheduleState::Failing
                    || job.run_count.unwrap_or(0) > 0,
                false,
                true,
            ),
            Some(p) => {
                let state_changed = p.state != state_str;
                let run_advanced = match (p.last_run_at, last_run_ms) {
                    (Some(old), Some(new)) => new > old,
                    (None, Some(_)) => true,
                    _ => false,
                };
                let meaningful_change = state_changed
                    || run_advanced
                    || resolved.run_observed
                    || p.enabled != job.enabled
                    || p.last_exit_code != job.last_exit_code
                    || p.last_error != job.last_error
                    || p.next_run_at != next_run_ms;
                (
                    state_changed || run_advanced || resolved.run_observed,
                    state_changed,
                    meaningful_change,
                )
            }
        };
        if job_changed {
            changed = true;
        }

        cx.db.execute(
            "INSERT INTO scheduled_jobs ( \
                 job_key, host, source, job_id, name, category, enabled, \
                 schedule_human, schedule_raw, expected_interval_secs, state, \
                 last_run_at, next_run_at, last_duration_ms, last_exit_code, \
                 run_count, run_count_observed_at, last_error, command_preview, \
                 deep_link, reported_at, updated_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22) \
             ON CONFLICT (job_key) DO UPDATE SET \
                 name = excluded.name, \
                 category = excluded.category, \
                 enabled = excluded.enabled, \
                 schedule_human = excluded.schedule_human, \
                 schedule_raw = excluded.schedule_raw, \
                 expected_interval_secs = excluded.expected_interval_secs, \
                 state = excluded.state, \
                 last_run_at = excluded.last_run_at, \
                 next_run_at = excluded.next_run_at, \
                 last_duration_ms = excluded.last_duration_ms, \
                 last_exit_code = excluded.last_exit_code, \
                 run_count = excluded.run_count, \
                 run_count_observed_at = excluded.run_count_observed_at, \
                 last_error = excluded.last_error, \
                 command_preview = excluded.command_preview, \
                 deep_link = excluded.deep_link, \
                 reported_at = excluded.reported_at, \
                 updated_at = excluded.updated_at",
            params![
                &job_key,
                host.as_str(),
                source_str,
                &job.id,
                &job.name,
                &category_str,
                i64::from(job.enabled),
                &job.schedule_human,
                &job.schedule_raw,
                expected_secs,
                &state_str,
                last_run_ms,
                next_run_ms,
                duration_ms,
                job.last_exit_code,
                run_count,
                resolved.run_count_observed_at,
                job.last_error.as_deref(),
                job.command_preview.as_deref(),
                job.deep_link.as_deref(),
                reported_ms,
                now_ms,
            ],
        )?;

        if should_record_history {
            let entry_at = if state_changed && !resolved.incoming_explains_run {
                reported_ms
            } else {
                last_run_ms.unwrap_or(reported_ms)
            };
            let error_summary = job
                .last_error
                .as_deref()
                .map(|e| truncate_utf8_bytes(e, 512));
            cx.db.execute(
                "INSERT INTO scheduled_job_history \
                 (job_key, at, state, exit_code, duration_ms, error_summary, run_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    &job_key,
                    entry_at,
                    &state_str,
                    job.last_exit_code,
                    duration_ms,
                    error_summary.as_deref(),
                    run_count,
                ],
            )?;
            // Enforce ring-buffer cap of 30 entries per job_key.
            cx.db.execute(
                "DELETE FROM scheduled_job_history \
                 WHERE job_key = ?1 \
                   AND id NOT IN ( \
                       SELECT id FROM scheduled_job_history \
                       WHERE job_key = ?1 ORDER BY at DESC, id DESC LIMIT ?2 \
                   )",
                params![&job_key, SCHEDULE_HISTORY_CAP as i64],
            )?;
            history_recorded += 1;
        }
    }

    // Prune jobs removed from this (host, source).
    let mut pruned: u32 = 0;
    for stale_key in existing.keys() {
        if !incoming_keys.contains(stale_key) {
            cx.db
                .execute("DELETE FROM scheduled_jobs WHERE job_key = ?1", [stale_key])?;
            pruned += 1;
            changed = true;
        }
    }

    if changed {
        cx.emit(
            None,
            actor,
            EventType::SchedulesUpdated,
            None,
            serde_json::json!({
                "host": host,
                "source": req.source,
                "upserted": req.jobs.len(),
                "pruned": pruned,
                "history_recorded": history_recorded,
            }),
        )?;
    }

    Ok(HostSchedulesAck {
        host: host.clone(),
        source: req.source,
        upserted: req.jobs.len() as u32,
        pruned,
        history_recorded,
        reported_at,
    })
}

/// Computes the effective state of a scheduled job against `now_ms`.
/// Upgrades enabled `ok` / `never_run` jobs to `overdue` once past their schedule + grace.
pub(crate) fn effective_schedule_state(
    stored: ScheduleState,
    enabled: bool,
    last_run_at: Option<i64>,
    next_run_at: Option<i64>,
    expected_interval_secs: Option<u64>,
    now_ms: i64,
) -> ScheduleState {
    if !enabled {
        return ScheduleState::Disabled;
    }
    if !matches!(stored, ScheduleState::Ok | ScheduleState::NeverRun) {
        return stored;
    }
    let grace_secs = expected_interval_secs
        .map(|i| (i / 4).clamp(120, 3_600))
        .unwrap_or(600);
    let grace_ms = (grace_secs as i64).saturating_mul(1_000);
    if let Some(next_ms) = next_run_at {
        if now_ms > next_ms.saturating_add(grace_ms) {
            return ScheduleState::Overdue;
        }
    } else if let (Some(last_ms), Some(interval)) = (last_run_at, expected_interval_secs)
        && interval > 0
    {
        let due_ms = last_ms
            .saturating_add((interval as i64).saturating_mul(1_000))
            .saturating_add(grace_ms);
        if now_ms > due_ms {
            return ScheduleState::Overdue;
        }
    }
    stored
}

/// Filter options for `GET /v1/schedules`.
#[derive(Debug, Default)]
pub(crate) struct ScheduleFilter {
    pub host: Option<DeviceName>,
    pub source: Option<ScheduleSource>,
    pub category: Option<ScheduleCategory>,
    pub status: Option<ScheduleState>,
    pub include_os: bool,
    pub q: Option<String>,
}

fn load_host_freshness(cx: &Ctx<'_>) -> Result<HashMap<DeviceName, (i64, i64)>> {
    let mut map: HashMap<DeviceName, (i64, i64)> = HashMap::new();
    let rows: Vec<(String, i64, i64)> = cx
        .db
        .prepare_cached("SELECT host, reported_at, ttl_secs FROM host_health_snapshots")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (host, reported_at, ttl_secs) in rows {
        let host: DeviceName = parse_id(&host)?;
        let expiry = reported_at.saturating_add(ttl_secs.saturating_mul(1_000));
        map.entry(host)
            .and_modify(|(rep, exp)| {
                if reported_at > *rep {
                    *rep = reported_at;
                }
                if expiry > *exp {
                    *exp = expiry;
                }
            })
            .or_insert((reported_at, expiry));
    }
    Ok(map)
}

fn load_all_schedules(cx: &Ctx<'_>) -> Result<Vec<ScheduledJobSnapshot>> {
    let now_ms = cx.now_ms();
    let freshness = load_host_freshness(cx)?;
    let rows: Vec<ScheduledJobSnapshot> = cx
        .db
        .prepare_cached(
            "SELECT job_key, host, source, job_id, name, category, enabled, \
                    schedule_human, schedule_raw, expected_interval_secs, state, \
                    last_run_at, next_run_at, last_duration_ms, last_exit_code, \
                    run_count, last_error, command_preview, deep_link, reported_at, updated_at \
             FROM scheduled_jobs ORDER BY host, source, name",
        )?
        .query_map([], |r| {
            let host: DeviceName = parse_id(&r.get::<_, String>(1)?)?;
            let source: ScheduleSource = parse_enum(&r.get::<_, String>(2)?)?;
            let category: ScheduleCategory = parse_enum(&r.get::<_, String>(5)?)?;
            let enabled = r.get::<_, i64>(6)? != 0;
            let expected_interval_secs = r.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u64);
            let stored_state: ScheduleState = parse_enum(&r.get::<_, String>(10)?)?;
            let last_run_ms: Option<i64> = r.get(11)?;
            let next_run_ms: Option<i64> = r.get(12)?;
            let reported_ms: i64 = r.get(19)?;
            let updated_ms: i64 = r.get(20)?;
            let state = effective_schedule_state(
                stored_state,
                enabled,
                last_run_ms,
                next_run_ms,
                expected_interval_secs,
                now_ms,
            );
            let host_stale = match freshness.get(&host) {
                Some((_, exp)) => now_ms > *exp,
                None => now_ms > reported_ms.saturating_add(600_000),
            };
            Ok(ScheduledJobSnapshot {
                job_key: r.get(0)?,
                host,
                source,
                id: r.get(3)?,
                name: r.get(4)?,
                category,
                enabled,
                schedule_human: r.get(7)?,
                schedule_raw: r.get(8)?,
                expected_interval_secs,
                state,
                last_run_at: last_run_ms.map(ts),
                next_run_at: next_run_ms.map(ts),
                last_duration_ms: r.get::<_, Option<i64>>(13)?.map(|v| v.max(0) as u64),
                last_exit_code: r.get(14)?,
                run_count: r.get::<_, Option<i64>>(15)?.map(|v| v.max(0) as u64),
                last_error: r.get(16)?,
                command_preview: r.get(17)?,
                deep_link: r.get(18)?,
                host_stale,
                reported_at: ts(reported_ms),
                updated_at: ts(updated_ms),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `GET /v1/schedules`: returns filtered scheduled jobs plus summary counts across the visible
/// (non-OS unless `include_os`) set.
pub(crate) fn list_schedules(
    cx: &Ctx<'_>,
    filter: &ScheduleFilter,
) -> Result<ScheduleListResponse> {
    let all = load_all_schedules(cx)?;
    let mut hosts_set: BTreeSet<DeviceName> = BTreeSet::new();
    let mut summary = ScheduleSummary::default();
    let mut visible_after_os = Vec::with_capacity(all.len());

    for job in all {
        hosts_set.insert(job.host.clone());
        if !filter.include_os && job.category == ScheduleCategory::Os {
            summary.os_hidden += 1;
            continue;
        }
        summary.total += 1;
        match job.state {
            ScheduleState::Failing => summary.failing += 1,
            ScheduleState::Overdue => summary.overdue += 1,
            ScheduleState::Running => summary.running += 1,
            ScheduleState::Ok => summary.ok += 1,
            ScheduleState::NeverRun => summary.never_run += 1,
            ScheduleState::Disabled => summary.disabled += 1,
            ScheduleState::Unknown => {}
        }
        visible_after_os.push(job);
    }

    let q_lower = filter
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let mut jobs: Vec<ScheduledJobSnapshot> = visible_after_os
        .into_iter()
        .filter(|j| filter.host.as_ref().is_none_or(|h| &j.host == h))
        .filter(|j| filter.source.is_none_or(|s| j.source == s))
        .filter(|j| filter.category.is_none_or(|c| j.category == c))
        .filter(|j| filter.status.is_none_or(|st| j.state == st))
        .filter(|j| {
            q_lower.as_ref().is_none_or(|q| {
                j.name.to_ascii_lowercase().contains(q)
                    || j.job_key.to_ascii_lowercase().contains(q)
                    || j.id.to_ascii_lowercase().contains(q)
                    || j.host.as_str().to_ascii_lowercase().contains(q)
                    || j.schedule_human.to_ascii_lowercase().contains(q)
                    || j.last_error
                        .as_deref()
                        .is_some_and(|e| e.to_ascii_lowercase().contains(q))
            })
        })
        .collect();

    // Problems-first default ordering: Failing -> Overdue -> Running -> Ok -> NeverRun -> Disabled.
    jobs.sort_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
    });

    Ok(ScheduleListResponse {
        generated_at: cx.now,
        summary,
        hosts: hosts_set.into_iter().collect(),
        jobs,
    })
}

fn state_rank(state: ScheduleState) -> u8 {
    match state {
        ScheduleState::Failing => 0,
        ScheduleState::Overdue => 1,
        ScheduleState::Running => 2,
        ScheduleState::Ok => 3,
        ScheduleState::NeverRun => 4,
        ScheduleState::Disabled => 5,
        ScheduleState::Unknown => 6,
    }
}

/// `GET /v1/schedules/{job_key}/history`: returns newest-first ring-buffered history entries.
pub(crate) fn schedule_history(cx: &Ctx<'_>, job_key: &str) -> Result<ScheduleHistoryResponse> {
    let exists: bool = cx.exists("SELECT 1 FROM scheduled_jobs WHERE job_key = ?1", [job_key])?;
    if !exists {
        return Err(not_found(
            format!("no scheduled job {job_key:?}"),
            "GET /v1/schedules lists the current scheduled jobs and their job_key values",
        ));
    }
    let entries: Vec<ScheduledJobHistoryEntry> = cx
        .db
        .prepare_cached(
            "SELECT id, at, state, exit_code, duration_ms, error_summary, run_count \
             FROM scheduled_job_history \
             WHERE job_key = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
        )?
        .query_map(params![job_key, SCHEDULE_HISTORY_CAP as i64], |r| {
            Ok(ScheduledJobHistoryEntry {
                id: r.get(0)?,
                at: ts(r.get(1)?),
                state: parse_enum(&r.get::<_, String>(2)?)?,
                exit_code: r.get(3)?,
                duration_ms: r.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
                error_summary: r.get(5)?,
                run_count: r.get::<_, Option<i64>>(6)?.map(|v| v.max(0) as u64),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ScheduleHistoryResponse {
        job_key: job_key.to_owned(),
        entries,
    })
}

/// `GET /v1/health/overview?days=7`: aggregates all 7 pillars into one bounded snapshot.
pub(crate) fn health_overview(
    cx: &Ctx<'_>,
    days: u32,
    server_commit: &str,
    started_at: Timestamp,
    db_path: &Path,
) -> Result<HealthOverview> {
    let now_ms = cx.now_ms();
    let schema_version: i64 = cx
        .db
        .pragma_query_value(None, "user_version", |r| r.get(0))?;
    let (db_bytes, wal_bytes) = db_file_sizes(db_path);
    let server = ServerHealthSummary {
        version: board_proto::VERSION.to_owned(),
        commit: server_commit.to_owned(),
        schema_version,
        uptime_secs: cx
            .now
            .signed_duration_since(started_at)
            .num_seconds()
            .max(0) as u64,
        db_bytes,
        wal_bytes,
        features: board_proto::SERVER_FEATURES
            .split(',')
            .map(str::to_owned)
            .collect(),
    };

    let devices = list_device_entries(cx)?;
    let usage_buckets = list_usage_buckets(cx)?;
    let boards = list_boards(cx)?;

    // 1. Factory, deploy train, lease hygiene, goal cooldowns.
    let factory = build_factory_health(cx, days, &boards)?;

    // 2. Token & cache efficiency analytics.
    let tokens = build_token_analytics(cx, days)?;

    // 3. Hosts & collectors merged across `host_health_snapshots` + `devices`.
    let (hosts, storage, backups, mut certs, alerts, vault, ci) =
        build_host_aggregates(cx, &devices, now_ms)?;

    // Append upcoming `not_before` tasks as generic expiry/deadline items so credential-rotation
    // or scheduled future tasks surface alongside TLS certs.
    append_upcoming_not_before_tasks(cx, now_ms, &mut certs)?;
    certs.sort_by(|a, b| match (a.item.expires_at, b.item.expires_at) {
        (Some(ta), Some(tb)) => ta.cmp(&tb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.item.name.cmp(&b.item.name),
    });

    // 4. Version skew detection across hosts and server commit.
    let version_skews = detect_version_skews(server_commit, &hosts);

    // 5. Scheduled job incident counts (excluding `os` category).
    let schedules = load_all_schedules(cx)?;
    let mut failing_jobs = 0u32;
    let mut overdue_jobs = 0u32;
    for job in &schedules {
        if job.category == ScheduleCategory::Os {
            continue;
        }
        match job.state {
            ScheduleState::Failing => failing_jobs += 1,
            ScheduleState::Overdue => overdue_jobs += 1,
            _ => {}
        }
    }

    // 6. Open asks count.
    let open_asks: u32 =
        cx.db
            .query_row("SELECT COUNT(*) FROM asks WHERE status = 'open'", [], |r| {
                r.get(0)
            })?;

    // 7. Alerts separation (`Watchdog` vs real firing alerts) & global posture.
    let mut firing_alerts = 0u32;
    let mut has_critical_alert = false;
    let mut watchdog_seen = false;
    for alert in &alerts {
        if alert.alert.is_watchdog || alert.alert.name.eq_ignore_ascii_case("watchdog") {
            watchdog_seen = true;
            continue;
        }
        firing_alerts += 1;
        if alert.alert.severity == AlertSeverity::Critical {
            has_critical_alert = true;
        }
    }
    let watchdog_ok = if alerts.is_empty() {
        None
    } else {
        Some(watchdog_seen)
    };

    let stale_hosts = hosts
        .iter()
        .filter(|h| h.status != DeviceStatus::Online)
        .count() as u32;
    let has_failed_backup = backups
        .iter()
        .any(|b| b.backup.state == BackupState::Failed);
    let has_sealed_vault = vault.iter().any(|v| !v.vault.reachable || v.vault.sealed);
    let has_offline_control = hosts.iter().any(|h| {
        h.status == DeviceStatus::Offline
            && matches!(
                h.device_role,
                Some(DeviceRole::Control | DeviceRole::ServiceHost)
            )
    }) || devices.iter().any(|d| {
        d.status == DeviceStatus::Offline
            && matches!(d.role, DeviceRole::Control | DeviceRole::ServiceHost)
    });
    let has_unhealthy_service = hosts.iter().any(|h| {
        h.services
            .iter()
            .any(|s| matches!(s.state, ServiceState::Failed))
            || h.containers.iter().any(|c| {
                c.health
                    .as_deref()
                    .is_some_and(|hc| hc.eq_ignore_ascii_case("unhealthy"))
            })
    });

    let posture = if has_critical_alert || has_sealed_vault || has_offline_control {
        HealthPosture::Incident
    } else if firing_alerts > 0
        || failing_jobs > 0
        || overdue_jobs > 0
        || stale_hosts > 0
        || !version_skews.is_empty()
        || has_failed_backup
        || has_unhealthy_service
        || factory.leases.stale_holder_count > 0
    {
        HealthPosture::Degraded
    } else {
        HealthPosture::Healthy
    };

    let incidents = IncidentCounts {
        firing_alerts,
        watchdog_ok,
        failing_jobs,
        overdue_jobs,
        stale_hosts,
        version_skews: version_skews.len() as u32,
        open_asks,
        stale_leases: factory.leases.stale_holder_count,
        open_feedback: factory.open_feedback,
    };

    Ok(HealthOverview {
        generated_at: cx.now,
        days,
        posture,
        incidents,
        server,
        factory,
        tokens,
        hosts,
        storage,
        backups,
        certs,
        alerts,
        vault,
        ci,
        version_skews,
        devices,
        usage_buckets,
    })
}

fn db_file_sizes(db_path: &Path) -> (Option<u64>, Option<u64>) {
    let db_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());
    let mut wal_os = db_path.as_os_str().to_os_string();
    wal_os.push("-wal");
    let wal_bytes = std::fs::metadata(Path::new(&wal_os)).ok().map(|m| m.len());
    (db_bytes, wal_bytes)
}

fn build_factory_health(cx: &Ctx<'_>, days: u32, boards: &[Board]) -> Result<FactoryHealthSummary> {
    let now_ms = cx.now_ms();
    let from_ms = now_ms.saturating_sub(i64::from(days) * DAY_MS);

    let active_runs: u32 = cx.db.query_row(
        "SELECT COUNT(*) FROM runs WHERE state IN ('queued', 'leased', 'starting', 'running', 'waiting_external')",
        [],
        |r| r.get(0),
    )?;

    let mut queue = QueueCounts::default();
    let active_boards: Vec<&Board> = boards
        .iter()
        .filter(|b| b.status == BoardStatus::Active)
        .collect();
    for b in &active_boards {
        let ready_auto = b.counts.ready_auto.unwrap_or(0);
        queue.ready_auto += ready_auto;
        queue.ready_manual += b.counts.ready.saturating_sub(ready_auto);
        queue.doing += b.counts.doing;
        queue.review += b.counts.review;
        queue.blocked += b.counts.blocked;
    }

    let (runs_total, runs_succeeded, runs_failed, runs_orphaned, fpm_yes, fpm_n, rr_sum, rr_n): (
        u32,
        u32,
        u32,
        u32,
        u32,
        u32,
        f64,
        u32,
    ) = cx.db.query_row(
        "SELECT COUNT(*), \
                COALESCE(SUM(CASE WHEN terminal_state = 'succeeded' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN terminal_state = 'failed' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN terminal_state = 'orphaned' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN first_pass_merge IS NOT NULL AND first_pass_merge != 0 THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN first_pass_merge IS NOT NULL THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN review_rounds IS NOT NULL THEN CAST(review_rounds AS REAL) ELSE 0.0 END), 0.0), \
                COALESCE(SUM(CASE WHEN review_rounds IS NOT NULL THEN 1 ELSE 0 END), 0) \
         FROM run_outcomes WHERE recorded_at >= ?1",
        [from_ms],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        },
    )?;

    let first_pass_merge_rate = (fpm_n > 0).then(|| f64::from(fpm_yes) / f64::from(fpm_n));
    let avg_review_rounds = (rr_n > 0).then(|| rr_sum / f64::from(rr_n));

    let deploy_trains = build_deploy_trains(cx, &active_boards)?;
    let leases = build_lease_hygiene(cx, now_ms)?;
    let goal_cooldowns = build_goal_cooldowns(cx, &active_boards, now_ms)?;

    let open_feedback: u32 = cx.db.query_row(
        "SELECT COUNT(*) FROM feedback f JOIN tasks t ON t.id = f.task \
         WHERE t.status NOT IN ('done', 'dropped') AND f.resolution IS NULL",
        [],
        |r| r.get(0),
    )?;

    Ok(FactoryHealthSummary {
        active_runs,
        queue,
        runs_total,
        runs_succeeded,
        runs_failed,
        runs_orphaned,
        first_pass_merge_rate,
        avg_review_rounds,
        deploy_trains,
        leases,
        goal_cooldowns,
        open_feedback,
    })
}

fn build_deploy_trains(cx: &Ctx<'_>, active_boards: &[&Board]) -> Result<Vec<DeployTrainStatus>> {
    let review_tasks = load_tasks(cx, "t.status = 'review'", &Vec::new())?;
    let boards_map: HashMap<&BoardSlug, &Board> =
        active_boards.iter().map(|b| (&b.slug, *b)).collect();

    // Active `repo:<r>:deploy` conflict locks.
    let deploy_locks: HashMap<String, String> = cx
        .db
        .prepare_cached("SELECT key, run_id FROM conflict_locks WHERE key LIKE 'repo:%:deploy'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    type DeployGroupKey = (BoardSlug, Option<String>);
    let mut groups: BTreeMap<DeployGroupKey, Vec<(TaskId, i64)>> = BTreeMap::new();
    for task in &review_tasks {
        let Some(board) = boards_map.get(&task.board) else {
            continue;
        };
        let Some(pass_note) = standing_review_pass(cx, &task.id)? else {
            continue;
        };
        if !has_after_merge_checks(&pass_note.text) {
            continue;
        }
        let Some(ready_at_ms) = after_merge_ready_at(cx, &task.id, pass_note.id)? else {
            continue;
        };
        let repo = task.repo.clone().or_else(|| board.repos.first().cloned());
        groups
            .entry((task.board.clone(), repo))
            .or_default()
            .push((task.id.clone(), ready_at_ms));
    }

    // Also include any active `repo:<r>:deploy` lock holder even if its tasks already closed.
    for lock_key in deploy_locks.keys() {
        if let Some(repo) = lock_key
            .strip_prefix("repo:")
            .and_then(|s| s.strip_suffix(":deploy"))
        {
            let matched = groups.keys().any(|(_, r)| r.as_deref() == Some(repo));
            if !matched
                && let Some(board) = active_boards
                    .iter()
                    .find(|b| b.repos.iter().any(|r| r == repo))
            {
                groups
                    .entry((board.slug.clone(), Some(repo.to_owned())))
                    .or_default();
            }
        }
    }

    let debounce_ms = cx.s.deploy_batch_debounce_ms();
    let max_wait_ms = cx.s.deploy_batch_max_wait_ms();

    let mut out = Vec::with_capacity(groups.len());
    for ((board, repo), mut entries) in groups {
        entries.sort_by_key(|(_, ms)| *ms);
        let oldest_ready_ms = entries.first().map(|(_, ms)| *ms);
        let newest_ready_ms = entries.last().map(|(_, ms)| *ms);
        let flush_ms = match (oldest_ready_ms, newest_ready_ms) {
            (Some(oldest), Some(newest)) if debounce_ms > 0 => {
                let quiet_at = newest.saturating_add(debounce_ms);
                let cap_at = oldest.saturating_add(max_wait_ms);
                Some(quiet_at.min(cap_at))
            }
            (Some(oldest), _) => Some(oldest),
            _ => None,
        };
        let lock_holder = repo
            .as_deref()
            .and_then(|r| deploy_locks.get(&format!("repo:{r}:deploy")))
            .cloned();

        // Latest completed deploy run on this board.
        let last_deploy: Option<(i64, String, Option<String>)> = cx
            .db
            .prepare_cached(
                "SELECT COALESCE(ended_at, updated_at), state, \
                        json_extract(outcome_json, '$.commit') \
                 FROM runs \
                 WHERE board = ?1 AND kind = 'merge' \
                   AND title LIKE 'after-merge-deploy %' \
                   AND state IN ('succeeded', 'failed', 'cancelled', 'orphaned') \
                 ORDER BY COALESCE(ended_at, updated_at) DESC LIMIT 1",
            )?
            .query_row([board.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;

        out.push(DeployTrainStatus {
            board,
            repo,
            pending_tasks: entries.into_iter().map(|(id, _)| id).collect(),
            oldest_ready_at: oldest_ready_ms.map(ts),
            flush_at: flush_ms.map(ts),
            active_lock_holder: lock_holder,
            last_deploy_at: last_deploy.as_ref().map(|(t, _, _)| ts(*t)),
            last_deploy_state: last_deploy.as_ref().map(|(_, s, _)| s.clone()),
            last_deploy_sha: last_deploy.and_then(|(_, _, sha)| sha),
        });
    }
    Ok(out)
}

fn build_lease_hygiene(cx: &Ctx<'_>, now_ms: i64) -> Result<LeaseHygieneSummary> {
    struct LeaseRow {
        id: i64,
        agent: String,
        task: Option<String>,
        repo: String,
        glob: String,
        expires_at: i64,
        agent_state: Option<String>,
        finished_managed: bool,
    }
    let rows: Vec<LeaseRow> = cx
        .db
        .prepare_cached(
            "SELECT l.id, l.agent, l.task, l.repo, l.glob, l.expires_at, a.state, \
                    EXISTS ( \
                        SELECT 1 FROM runs r \
                        WHERE r.agent_id = l.agent AND r.origin = 'managed' \
                          AND r.state IN ('succeeded', 'failed', 'cancelled', 'orphaned') \
                    ) AND NOT EXISTS ( \
                        SELECT 1 FROM runs r \
                        WHERE r.agent_id = l.agent \
                          AND r.state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
                    ) AS finished_managed \
             FROM leases l \
             LEFT JOIN agents a ON a.id = l.agent \
             WHERE l.expires_at > ?1 \
             ORDER BY l.expires_at ASC",
        )?
        .query_map([now_ms], |r| {
            Ok(LeaseRow {
                id: r.get(0)?,
                agent: r.get(1)?,
                task: r.get(2)?,
                repo: r.get(3)?,
                glob: r.get(4)?,
                expires_at: r.get(5)?,
                agent_state: r.get(6)?,
                finished_managed: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let active_count = rows.len() as u32;
    let mut stale_holder_count = 0u32;
    let mut items = Vec::new();
    for row in rows {
        let stale_holder = row.finished_managed || row.agent_state.as_deref() == Some("ended");
        if stale_holder {
            stale_holder_count += 1;
        }
        if items.len() < 20 {
            items.push(LeaseHygieneItem {
                id: row.id,
                agent: row.agent,
                task: row.task.as_deref().map(parse_id).transpose()?,
                repo: row.repo,
                glob: row.glob,
                expires_at: ts(row.expires_at),
                stale_holder,
            });
        }
    }
    Ok(LeaseHygieneSummary {
        active_count,
        stale_holder_count,
        items,
    })
}

fn build_goal_cooldowns(
    cx: &Ctx<'_>,
    active_boards: &[&Board],
    now_ms: i64,
) -> Result<Vec<GoalCooldownStatus>> {
    let mut out = Vec::new();
    for b in active_boards {
        if !b.autopilot && b.goal_review_every_secs.is_none() {
            continue;
        }
        let last_goal_ms: Option<i64> = cx
            .db
            .prepare_cached(
                "SELECT MAX(COALESCE(started_at, created_at)) FROM runs \
                 WHERE board = ?1 AND kind = 'goal'",
            )?
            .query_row([b.slug.as_str()], |r| r.get(0))?;
        let (next_eligible_at, cooldown_remaining_secs) =
            match (last_goal_ms, b.goal_review_every_secs) {
                (Some(last), Some(every)) if every > 0 => {
                    let next_ms = last.saturating_add((every as i64).saturating_mul(1_000));
                    let rem_secs = ((next_ms - now_ms).max(0) / 1_000) as u64;
                    (Some(ts(next_ms)), Some(rem_secs))
                }
                _ => (None, None),
            };
        out.push(GoalCooldownStatus {
            board: b.slug.clone(),
            autopilot: b.autopilot,
            every_secs: b.goal_review_every_secs,
            last_goal_at: last_goal_ms.map(ts),
            next_eligible_at,
            cooldown_remaining_secs,
        });
    }
    Ok(out)
}

fn build_token_analytics(cx: &Ctx<'_>, days: u32) -> Result<TokenAnalyticsSummary> {
    let from_ms = cx.now_ms().saturating_sub(i64::from(days) * DAY_MS);

    #[derive(Default)]
    struct Agg {
        runs: u32,
        tokens_in: u64,
        cached_tokens: u64,
        tokens_out: u64,
        cache_num: u64,
        cache_den: u64,
    }
    impl Agg {
        fn add(&mut self, ti: u64, ct: u64, to: u64, has_cached: bool) {
            self.runs += 1;
            self.tokens_in = self.tokens_in.saturating_add(ti);
            self.cached_tokens = self.cached_tokens.saturating_add(ct);
            self.tokens_out = self.tokens_out.saturating_add(to);
            if ti > 0 && has_cached {
                self.cache_num = self.cache_num.saturating_add(ct.min(ti));
                self.cache_den = self.cache_den.saturating_add(ti);
            }
        }
        fn billable(&self) -> u64 {
            self.tokens_in
                .saturating_sub(self.cached_tokens.min(self.tokens_in))
        }
        fn cache_hit_rate(&self) -> Option<f64> {
            (self.cache_den > 0)
                .then(|| (self.cache_num as f64 / self.cache_den as f64).clamp(0.0, 1.0))
        }
    }

    struct TokenRow {
        rec_ms: i64,
        board: Option<String>,
        kind: String,
        device: Option<String>,
        model: Option<String>,
        ti: Option<i64>,
        ct: Option<i64>,
        to: Option<i64>,
        rt: Option<i64>,
    }

    let rows: Vec<TokenRow> = cx
        .db
        .prepare_cached(
            "SELECT ro.recorded_at, ro.board, ro.kind, ro.device, \
                    COALESCE(json_extract(r.outcome_json, '$.model'), r.runner) AS model, \
                    ro.tokens_in, ro.cached_tokens, ro.tokens_out, ro.reasoning_tokens \
             FROM run_outcomes ro \
             LEFT JOIN runs r ON r.id = ro.run_id \
             WHERE ro.recorded_at >= ?1 \
               AND (ro.tokens_in IS NOT NULL OR ro.cached_tokens IS NOT NULL \
                    OR ro.tokens_out IS NOT NULL OR ro.reasoning_tokens IS NOT NULL)",
        )?
        .query_map([from_ms], |r| {
            Ok(TokenRow {
                rec_ms: r.get(0)?,
                board: r.get(1)?,
                kind: r.get(2)?,
                device: r.get(3)?,
                model: r.get(4)?,
                ti: r.get(5)?,
                ct: r.get(6)?,
                to: r.get(7)?,
                rt: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut total = Agg::default();
    let mut reasoning_tokens: u64 = 0;
    let mut by_day: BTreeMap<String, Agg> = BTreeMap::new();
    let mut by_board: BTreeMap<String, Agg> = BTreeMap::new();
    let mut by_kind: BTreeMap<String, Agg> = BTreeMap::new();
    let mut by_device: BTreeMap<String, Agg> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Agg> = BTreeMap::new();

    for row in rows {
        let has_cached = row.ct.is_some();
        let ti = row.ti.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        let ct = row.ct.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        let to = row.to.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        let rt = row.rt.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        reasoning_tokens = reasoning_tokens.saturating_add(rt);

        total.add(ti, ct, to, has_cached);
        let day_key = ts(row.rec_ms).format("%Y-%m-%d").to_string();
        by_day
            .entry(day_key)
            .or_default()
            .add(ti, ct, to, has_cached);
        by_board
            .entry(row.board.unwrap_or_else(|| "unassigned".to_owned()))
            .or_default()
            .add(ti, ct, to, has_cached);
        by_kind
            .entry(row.kind)
            .or_default()
            .add(ti, ct, to, has_cached);
        by_device
            .entry(row.device.unwrap_or_else(|| "unknown".to_owned()))
            .or_default()
            .add(ti, ct, to, has_cached);
        by_model
            .entry(row.model.unwrap_or_else(|| "unknown".to_owned()))
            .or_default()
            .add(ti, ct, to, has_cached);
    }

    let to_rows = |map: BTreeMap<String, Agg>| -> Vec<TokenBreakdownRow> {
        let mut list: Vec<TokenBreakdownRow> = map
            .into_iter()
            .map(|(key, agg)| {
                let billable_in = agg.billable();
                let avg_billable_per_run = if agg.runs > 0 {
                    billable_in / u64::from(agg.runs)
                } else {
                    0
                };
                TokenBreakdownRow {
                    key,
                    runs: agg.runs,
                    tokens_in: agg.tokens_in,
                    billable_in,
                    cached_tokens: agg.cached_tokens,
                    tokens_out: agg.tokens_out,
                    cache_hit_rate: agg.cache_hit_rate(),
                    avg_billable_per_run,
                }
            })
            .collect();
        list.sort_by(|a, b| {
            b.billable_in
                .cmp(&a.billable_in)
                .then_with(|| b.tokens_in.cmp(&a.tokens_in))
                .then_with(|| a.key.cmp(&b.key))
        });
        list
    };

    let daily: Vec<DailyTokenBucket> = by_day
        .into_iter()
        .map(|(day, agg)| DailyTokenBucket {
            day,
            runs: agg.runs,
            tokens_in: agg.tokens_in,
            billable_in: agg.billable(),
            cached_tokens: agg.cached_tokens,
            tokens_out: agg.tokens_out,
            cache_hit_rate: agg.cache_hit_rate(),
        })
        .collect();

    Ok(TokenAnalyticsSummary {
        days,
        runs_with_tokens: total.runs,
        tokens_in: total.tokens_in,
        billable_in: total.billable(),
        cached_tokens: total.cached_tokens,
        tokens_out: total.tokens_out,
        reasoning_tokens,
        cache_hit_rate: total.cache_hit_rate(),
        daily,
        by_board: to_rows(by_board),
        by_kind: to_rows(by_kind),
        by_device: to_rows(by_device),
        by_model: to_rows(by_model),
    })
}

type HostAggregates = (
    Vec<HostHealthSummary>,
    Vec<HostStorageQuota>,
    Vec<HostBackupStatus>,
    Vec<HostExpiryItem>,
    Vec<HostExternalAlert>,
    Vec<HostVaultHealth>,
    Vec<HostCiHealth>,
);

fn build_host_aggregates(
    cx: &Ctx<'_>,
    devices: &[DeviceEntry],
    now_ms: i64,
) -> Result<HostAggregates> {
    let device_map: HashMap<&DeviceName, &DeviceEntry> =
        devices.iter().map(|d| (&d.name, d)).collect();

    struct SnapRow {
        host: DeviceName,
        collector: String,
        report: HostHealthReport,
        reported_at: i64,
        ttl_secs: i64,
    }

    let snaps: Vec<SnapRow> = cx
        .db
        .prepare_cached(
            "SELECT host, collector, report_json, reported_at, ttl_secs \
             FROM host_health_snapshots ORDER BY host, collector",
        )?
        .query_map([], |r| {
            Ok(SnapRow {
                host: parse_id(&r.get::<_, String>(0)?)?,
                collector: r.get(1)?,
                report: parse_json(&r.get::<_, String>(2)?)?,
                reported_at: r.get(3)?,
                ttl_secs: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut by_host: BTreeMap<DeviceName, Vec<SnapRow>> = BTreeMap::new();
    for snap in snaps {
        by_host.entry(snap.host.clone()).or_default().push(snap);
    }

    // Load recent hourly resource samples for all hosts in one pass.
    let mut samples_by_host: HashMap<DeviceName, Vec<HostResourceSample>> = HashMap::new();
    let sample_cutoff = now_ms.saturating_sub(SAMPLES_PER_HOST_MAX * HOUR_MS);
    let sample_rows: Vec<(DeviceName, HostResourceSample)> = cx
        .db
        .prepare_cached(
            "SELECT host, hour_bucket, cpu_pct, mem_used_pct, disk_used_pct, disk_free_gib \
             FROM host_health_samples WHERE hour_bucket >= ?1 ORDER BY host, hour_bucket ASC",
        )?
        .query_map([sample_cutoff], |r| {
            Ok((
                parse_id(&r.get::<_, String>(0)?)?,
                HostResourceSample {
                    hour_bucket: ts(r.get(1)?),
                    cpu_pct: r.get(2)?,
                    mem_used_pct: r.get(3)?,
                    disk_used_pct: r.get(4)?,
                    disk_free_gib: r.get(5)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (host, sample) in sample_rows {
        samples_by_host.entry(host).or_default().push(sample);
    }

    let mut hosts = Vec::new();
    let mut storage = Vec::new();
    let mut backups = Vec::new();
    let mut certs = Vec::new();
    let mut alerts = Vec::new();
    let mut vault = Vec::new();
    let mut ci = Vec::new();

    for (host, mut group) in by_host {
        // Put the primary `"host"` collector first so its resources/os/uptime take precedence.
        group.sort_by(|a, b| {
            let ap = a.collector != "host";
            let bp = b.collector != "host";
            ap.cmp(&bp).then_with(|| a.collector.cmp(&b.collector))
        });

        let dev = device_map.get(&host).copied();
        let mut os: Option<String> = None;
        let mut arch: Option<String> = None;
        let mut uptime_secs = None;
        let mut newest_reported_ms: Option<i64> = None;
        let mut any_fresh = false;
        let mut any_within_offline_grace = false;
        let mut collectors = Vec::new();
        let mut resources = None;
        let mut network = None;
        let mut versions = BTreeMap::new();
        let mut containers = Vec::new();
        let mut services = Vec::new();

        for snap in group {
            collectors.push(snap.collector);
            if newest_reported_ms.is_none_or(|prev| snap.reported_at > prev) {
                newest_reported_ms = Some(snap.reported_at);
            }
            let ttl_ms = snap.ttl_secs.saturating_mul(1_000);
            if now_ms <= snap.reported_at.saturating_add(ttl_ms) {
                any_fresh = true;
                any_within_offline_grace = true;
            } else if now_ms <= snap.reported_at.saturating_add(ttl_ms.saturating_mul(3)) {
                any_within_offline_grace = true;
            }
            // Primary `"host"` collector is sorted first; secondary collectors only fill gaps.
            if os.is_none() && snap.report.os.is_some() {
                os = snap.report.os;
            }
            if arch.is_none() && snap.report.arch.is_some() {
                arch = snap.report.arch;
            }
            if uptime_secs.is_none() && snap.report.uptime_secs.is_some() {
                uptime_secs = snap.report.uptime_secs;
            }
            if resources.is_none() && snap.report.resources.is_some() {
                resources = snap.report.resources;
            }
            if let Some(net) = snap.report.network {
                match &mut network {
                    None => network = Some(net),
                    Some(existing) => {
                        if existing.tailscale_online.is_none() && net.tailscale_online.is_some() {
                            existing.tailscale_online = net.tailscale_online;
                        }
                        if existing.tailscale_mode.is_none() && net.tailscale_mode.is_some() {
                            existing.tailscale_mode = net.tailscale_mode;
                        }
                        if existing.tailscale_latency_ms.is_none()
                            && net.tailscale_latency_ms.is_some()
                        {
                            existing.tailscale_latency_ms = net.tailscale_latency_ms;
                        }
                        if existing.heartbeat_ok.is_none() && net.heartbeat_ok.is_some() {
                            existing.heartbeat_ok = net.heartbeat_ok;
                        }
                        if existing.heartbeat_checked_at.is_none()
                            && net.heartbeat_checked_at.is_some()
                        {
                            existing.heartbeat_checked_at = net.heartbeat_checked_at;
                        }
                        if existing.heartbeat_label.is_none() && net.heartbeat_label.is_some() {
                            existing.heartbeat_label = net.heartbeat_label;
                        }
                    }
                }
            }
            for (k, v) in snap.report.versions {
                versions.entry(k).or_insert(v);
            }
            containers.extend(snap.report.containers);
            services.extend(snap.report.services);
            for q in snap.report.storage {
                storage.push(HostStorageQuota {
                    host: host.clone(),
                    quota: q,
                });
            }
            for b in snap.report.backups {
                backups.push(HostBackupStatus {
                    host: host.clone(),
                    backup: b,
                });
            }
            for c in snap.report.certs {
                certs.push(HostExpiryItem {
                    host: host.clone(),
                    item: c,
                });
            }
            for a in snap.report.alerts {
                alerts.push(HostExternalAlert {
                    host_reporter: host.clone(),
                    alert: a,
                });
            }
            if let Some(v) = snap.report.vault {
                vault.push(HostVaultHealth {
                    host: host.clone(),
                    vault: v,
                });
            }
            if let Some(c) = snap.report.ci {
                ci.push(HostCiHealth {
                    host: host.clone(),
                    ci: c,
                });
            }
        }

        if os.is_none() {
            os = dev.and_then(|d| d.os.clone());
        }
        if arch.is_none() {
            arch = dev.and_then(|d| d.arch.clone());
        }

        let status = if any_fresh {
            DeviceStatus::Online
        } else if any_within_offline_grace {
            DeviceStatus::Stale
        } else if newest_reported_ms.is_some() {
            DeviceStatus::Offline
        } else {
            dev.map_or(DeviceStatus::Offline, |d| d.status)
        };

        let samples = samples_by_host.remove(&host).unwrap_or_default();
        hosts.push(HostHealthSummary {
            host,
            device_role: dev.map(|d| d.role),
            status,
            os,
            arch,
            uptime_secs,
            reported_at: newest_reported_ms.map(ts),
            collectors,
            resources,
            network,
            versions,
            containers,
            services,
            samples,
        });
    }

    // Sort alerts: non-watchdog critical first, warning next, info next, watchdog last.
    alerts.sort_by(|a, b| {
        let rank = |x: &HostExternalAlert| -> u8 {
            if x.alert.is_watchdog || x.alert.name.eq_ignore_ascii_case("watchdog") {
                return 9;
            }
            match x.alert.severity {
                AlertSeverity::Critical => 0,
                AlertSeverity::Warning => 1,
                AlertSeverity::Info => 2,
                AlertSeverity::Unknown => 3,
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.alert.name.cmp(&b.alert.name))
    });

    Ok((hosts, storage, backups, certs, alerts, vault, ci))
}

fn append_upcoming_not_before_tasks(
    cx: &Ctx<'_>,
    now_ms: i64,
    certs: &mut Vec<HostExpiryItem>,
) -> Result<()> {
    let rows: Vec<(String, String, String, i64)> = cx
        .db
        .prepare_cached(
            "SELECT id, board, title, not_before FROM tasks \
             WHERE status = 'open' AND not_before IS NOT NULL AND not_before > ?1 \
             ORDER BY not_before ASC LIMIT 20",
        )?
        .query_map([now_ms], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let board_host: DeviceName = "board".parse().expect("valid slug");
    for (id, board, title, not_before_ms) in rows {
        certs.push(HostExpiryItem {
            host: board_host.clone(),
            item: ExpiryItem {
                name: format!("{id}: {title}"),
                kind: "task_not_before".to_owned(),
                expires_at: Some(ts(not_before_ms)),
                warn_days: Some(14),
                ok: true,
                detail: Some(format!("board {board}")),
            },
        });
    }
    Ok(())
}

fn detect_version_skews(server_commit: &str, hosts: &[HostHealthSummary]) -> Vec<VersionSkew> {
    let mut by_component: BTreeMap<String, Vec<HostVersionEntry>> = BTreeMap::new();
    for h in hosts {
        for (comp, ver) in &h.versions {
            by_component
                .entry(comp.clone())
                .or_default()
                .push(HostVersionEntry {
                    host: h.host.clone(),
                    version: ver.clone(),
                });
        }
    }
    let mut skews = Vec::new();
    for (component, entries) in by_component {
        let expected = if (component == "board" || component == "board-mcp")
            && !server_commit.is_empty()
            && server_commit != "unknown"
        {
            server_commit.to_owned()
        } else {
            // Expected is the highest/majority version reported across hosts.
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for e in &entries {
                *counts.entry(e.version.as_str()).or_insert(0) += 1;
            }
            match counts
                .into_iter()
                .max_by(|(va, ca), (vb, cb)| ca.cmp(cb).then_with(|| va.cmp(vb)))
            {
                Some((v, _)) => v.to_owned(),
                None => continue,
            }
        };
        let mismatched: Vec<HostVersionEntry> = entries
            .into_iter()
            .filter(|e| e.version != expected)
            .collect();
        if !mismatched.is_empty() {
            skews.push(VersionSkew {
                component,
                expected,
                mismatched,
            });
        }
    }
    skews
}
