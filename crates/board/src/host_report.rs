//! Opt-in host health and scheduled-job collectors (`board report-host` and periodic
//! `board watch` background reporting).
//!
//! Everything here is generic and config-driven (`[telemetry]` and `[schedules]` in
//! `config.toml` or a standalone `--config` TOML file). Secrets are redacted at the
//! collector edge before any snippet leaves the host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use board_proto::{
    ContainerStatus, DeviceName, HostHealthReport, HostNetworkStatus, HostResources,
    PostHostSchedules, SCHEDULE_COMMAND_CHARS_MAX, SCHEDULE_ERROR_BYTES_MAX, ScheduleCategory,
    ScheduleSource, ScheduleState, ScheduledJobReport, ServiceState, ServiceStatus, Timestamp,
};
use chrono::{DateTime, Datelike as _, Local, TimeDelta, TimeZone as _, Timelike as _, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::{Client, is_endpoint_unsupported};
use crate::config::{Config, Home, SchedulesConfig, TelemetryConfig};
use crate::daemon::Shutdown;

/// Built-in Linux OS maintenance timer prefixes classified as [`ScheduleCategory::Os`].
const DEFAULT_OS_TIMER_PREFIXES: &[&str] = &[
    "apt-daily",
    "dpkg-db-backup",
    "e2scrub_all",
    "fstrim",
    "fwupd",
    "logrotate",
    "man-db",
    "motd-news",
    "phpsessionclean",
    "plocate-updatedb",
    "shadow",
    "sysstat",
    "systemd-tmpfiles",
    "ua-timer",
    "ubuntu-advantage",
    "unattended-upgrades",
    "update-notifier",
];

/// Standalone TOML config file accepted by `board report-host --config <path>`.
/// Inherits `server`, `device`, and token from `$BOARD_HOME` when omitted.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostReportFileConfig {
    pub server: Option<String>,
    pub device: Option<DeviceName>,
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
    #[serde(default)]
    pub schedules: Option<SchedulesConfig>,
}

/// Collected payload from one host inspection pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedHostReport {
    pub health: HostHealthReport,
    pub schedules: Vec<PostHostSchedules>,
}

/// Result of pushing a collected report to the board server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportPushSummary {
    pub host: DeviceName,
    pub collector: String,
    pub health_pushed: bool,
    pub schedule_sources: Vec<String>,
    pub total_jobs: usize,
}

/// Whether periodic reporting is enabled in `config`.
pub fn is_enabled(config: &Config) -> bool {
    config.telemetry.as_ref().is_some_and(|t| t.enabled)
        || config.schedules.as_ref().is_some_and(|s| s.enabled)
}

/// Loads a standalone `--config` TOML file.
pub fn load_file_config(path: &Path) -> Result<HostReportFileConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading host-report config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing host-report config {}", path.display()))
}

/// Collects host health and schedules according to `telemetry` and `schedules` configs.
pub fn collect(
    telemetry: Option<&TelemetryConfig>,
    schedules: Option<&SchedulesConfig>,
    now: Timestamp,
) -> CollectedHostReport {
    let tel = telemetry.cloned().unwrap_or_default();
    let mut health = HostHealthReport {
        collector: if tel.collector.trim().is_empty() {
            "host".to_owned()
        } else {
            tel.collector.clone()
        },
        ttl_secs: tel.ttl_secs.clamp(
            board_proto::HOST_HEALTH_TTL_SECS_MIN,
            board_proto::HOST_HEALTH_TTL_SECS_MAX,
        ),
        reported_at: Some(now),
        os: Some(crate::daemon::device::host_os().to_owned()),
        arch: Some(crate::daemon::device::host_arch().to_owned()),
        sleepable: tel.sleepable.then_some(true),
        uptime_secs: collect_uptime_secs(),
        resources: if tel.resources {
            collect_host_resources(tel.cpu_cap_slice.as_deref())
        } else {
            None
        },
        network: if tel.tailscale {
            collect_tailscale_status()
        } else {
            None
        },
        versions: collect_versions(&tel.version_probes),
        containers: if tel.docker {
            collect_docker_containers()
        } else {
            Vec::new()
        },
        services: collect_services(&tel),
        ..HostHealthReport::default()
    };
    if health.versions.is_empty() && crate::COMMIT != "unknown" {
        health
            .versions
            .insert("board".to_owned(), crate::COMMIT.to_owned());
    }

    let mut schedule_batches = Vec::new();
    if let Some(sc) = schedules
        && sc.enabled
    {
        if sc.launchd {
            let jobs = collect_launchd_schedules(sc, now);
            schedule_batches.push(PostHostSchedules {
                source: ScheduleSource::Launchd,
                reported_at: Some(now),
                jobs,
            });
        }
        if sc.systemd_timers {
            let jobs = collect_systemd_timers(sc);
            schedule_batches.push(PostHostSchedules {
                source: ScheduleSource::Systemd,
                reported_at: Some(now),
                jobs,
            });
        }
        if sc.crontab {
            let jobs = collect_crontab_schedules(sc);
            schedule_batches.push(PostHostSchedules {
                source: ScheduleSource::Cron,
                reported_at: Some(now),
                jobs,
            });
        }
        if let Some(db_path) = &sc.t3_state_db {
            match collect_t3_schedules(db_path, sc) {
                Ok(jobs) => schedule_batches.push(PostHostSchedules {
                    source: ScheduleSource::T3,
                    reported_at: Some(now),
                    jobs,
                }),
                Err(err) => {
                    tracing::debug!("reading T3 scheduled_tasks {}: {err:#}", db_path.display());
                }
            }
        }
    }

    CollectedHostReport {
        health,
        schedules: schedule_batches,
    }
}

/// Collects and pushes health + schedule batches to the server.
pub async fn collect_and_push(
    client: &Client,
    host: &DeviceName,
    telemetry: Option<&TelemetryConfig>,
    schedules: Option<&SchedulesConfig>,
) -> Result<ReportPushSummary> {
    let tel = telemetry.cloned();
    let sch = schedules.cloned();
    let collected =
        tokio::task::spawn_blocking(move || collect(tel.as_ref(), sch.as_ref(), Utc::now()))
            .await
            .context("host report collector task failed")?;

    let mut health_pushed = false;
    if telemetry.is_some_and(|t| t.enabled) || schedules.is_none_or(|s| !s.enabled) {
        client.post_host_health(host, &collected.health).await?;
        health_pushed = true;
    }

    let mut schedule_sources = Vec::new();
    let mut total_jobs = 0usize;
    for batch in &collected.schedules {
        client.post_host_schedules(host, batch).await?;
        schedule_sources.push(batch.source.as_str().to_owned());
        total_jobs += batch.jobs.len();
    }

    Ok(ReportPushSummary {
        host: host.clone(),
        collector: collected.health.collector,
        health_pushed,
        schedule_sources,
        total_jobs,
    })
}

/// Spawns the periodic host telemetry & schedule reporter inside `board watch`.
/// Stays idle whenever neither `[telemetry]` nor `[schedules]` is enabled in `config_rx`.
pub fn spawn_periodic(
    client: Client,
    device: DeviceName,
    config_rx: watch::Receiver<Config>,
    mut shutdown: Shutdown,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut logged_unsupported = false;
        // Brief initial jitter so startup heartbeats/sync run first.
        tokio::select! {
            () = shutdown.wait() => return,
            () = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
        loop {
            let cfg = config_rx.borrow().clone();
            let interval_secs = cfg
                .telemetry
                .as_ref()
                .map(|t| t.interval_secs.max(30))
                .unwrap_or(60);
            if is_enabled(&cfg) {
                match collect_and_push(
                    &client,
                    &device,
                    cfg.telemetry.as_ref(),
                    cfg.schedules.as_ref(),
                )
                .await
                {
                    Ok(summary) => {
                        logged_unsupported = false;
                        tracing::debug!(
                            host = %summary.host,
                            jobs = summary.total_jobs,
                            "host health/schedules reported"
                        );
                    }
                    Err(err) if is_endpoint_unsupported(&err) => {
                        if !logged_unsupported {
                            tracing::info!(
                                "board server does not expose /v1/hosts/{device}/health yet; pausing host telemetry"
                            );
                            logged_unsupported = true;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("periodic host report failed: {err:#}");
                    }
                }
            }
            let wait = if logged_unsupported {
                Duration::from_secs(300)
            } else {
                Duration::from_secs(interval_secs)
            };
            tokio::select! {
                () = shutdown.wait() => break,
                () = tokio::time::sleep(wait) => {}
            }
        }
    })
}

/// Runs `board report-host` from the CLI.
pub async fn run_cli(
    home: &Home,
    config_path: Option<&Path>,
    host_override: Option<DeviceName>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let base_config = Config::load(home)?;
    let file_cfg = match config_path {
        Some(path) => Some(load_file_config(path)?),
        None => None,
    };

    let telemetry = file_cfg
        .as_ref()
        .and_then(|f| f.telemetry.clone())
        .or(base_config.telemetry.clone())
        .unwrap_or_else(|| TelemetryConfig {
            enabled: true,
            ..TelemetryConfig::default()
        });
    let schedules = file_cfg
        .as_ref()
        .and_then(|f| f.schedules.clone())
        .or(base_config.schedules.clone());

    if dry_run {
        let collected = collect(Some(&telemetry), schedules.as_ref(), Utc::now());
        println!("{}", serde_json::to_string_pretty(&collected)?);
        return Ok(());
    }

    let server = file_cfg
        .as_ref()
        .and_then(|f| f.server.clone())
        .or(base_config.server.clone())
        .context("no server configured: pass --config with `server = ...` or run `board login`")?;
    let host = host_override
        .or_else(|| file_cfg.as_ref().and_then(|f| f.device.clone()))
        .or(base_config.device.clone())
        .context(
            "no device/host configured: pass --host, set `device` in config, or run `board login`",
        )?;
    let token = match file_cfg.as_ref().and_then(|f| f.token_file.as_ref()) {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading token_file {}", path.display()))?
            .trim()
            .to_owned(),
        None => crate::config::load_token(home)?
            .context("no device token found: run `board login` or set `token_file` in --config")?,
    };

    let client = Client::new(&server, token, None, Some(Duration::from_secs(30)))?;
    let summary = collect_and_push(&client, &host, Some(&telemetry), schedules.as_ref()).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "reported host {} (collector={}, schedules=[{}], jobs={})",
            summary.host,
            summary.collector,
            summary.schedule_sources.join(","),
            summary.total_jobs
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Secret redaction & bounded error tails
// ---------------------------------------------------------------------------

/// Strips ANSI escapes, control chars, Bearer tokens, URL credentials, env/CLI/JSON secret
/// values, and known token prefixes before telemetry leaves the host.
pub fn redact_secrets(raw: &str) -> String {
    let cleaned = strip_ansi_and_controls(raw);
    let mut out_lines = Vec::new();
    for line in cleaned.lines() {
        out_lines.push(redact_line(line));
    }
    out_lines.join("\n")
}

/// Redacts secrets and retains the tail (last `max_lines` lines, at most `max_bytes` UTF-8 bytes)
/// so the actual failure line at the end of a log is preserved.
pub fn redact_tail(raw: &str, max_lines: usize, max_bytes: usize) -> Option<String> {
    let redacted = redact_secrets(raw);
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let joined = lines[start..].join("\n");
    if joined.len() <= max_bytes {
        return Some(joined);
    }
    let mut byte_start = joined.len() - max_bytes;
    while byte_start < joined.len() && !joined.is_char_boundary(byte_start) {
        byte_start += 1;
    }
    Some(joined[byte_start..].to_owned())
}

fn strip_ansi_and_controls(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}'
                            || (next == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                        {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn redact_line(line: &str) -> String {
    // 1. Redact URL credentials: scheme://user:pass@host -> scheme://<redacted>@host.
    let mut text = redact_url_credentials(line);

    // 2. Redact JSON secret keys: `"token": "..."`, `"api_key": "..."`, etc.
    text = redact_json_secret_fields(&text);

    // 3. Token-by-token redaction for Bearer headers, CLI flags, env vars, and known prefixes.
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;

    for word in text.split_ascii_whitespace() {
        let lower = word.to_ascii_lowercase();
        if redact_next {
            // Preserve the auth scheme after `Authorization:` and keep `redact_next = true`
            // so the credential token that follows the scheme is redacted.
            if matches!(lower.as_str(), "bearer" | "basic" | "token" | "digest") {
                out.push(word.to_owned());
                continue;
            }
            out.push("<redacted>".to_owned());
            redact_next = false;
            continue;
        }
        if matches!(lower.as_str(), "bearer" | "basic" | "token" | "digest") {
            out.push(word.to_owned());
            redact_next = true;
            continue;
        }
        if matches!(
            lower.as_str(),
            "--token"
                | "--secret"
                | "--password"
                | "--api-key"
                | "--apikey"
                | "-p"
                | "authorization:"
                | "x-api-key:"
        ) {
            out.push(word.to_owned());
            redact_next = true;
            continue;
        }
        if let Some((lhs, _rhs)) = word.split_once('=')
            && is_secret_key_name(lhs)
        {
            out.push(format!("{lhs}=<redacted>"));
            continue;
        }
        if let Some((lhs, rhs)) = word.split_once(':')
            && !rhs.is_empty()
            && !rhs.starts_with("//")
            && is_secret_key_name(lhs)
        {
            out.push(format!("{lhs}:<redacted>"));
            continue;
        }
        if looks_like_secret_token(word) {
            out.push("<redacted>".to_owned());
            continue;
        }
        out.push(word.to_owned());
    }
    out.join(" ")
}

fn is_secret_key_name(raw: &str) -> bool {
    let key = raw
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_ascii_lowercase();
    if key.is_empty() {
        return false;
    }
    const SENSITIVE: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "api_key",
        "api-key",
        "auth",
        "authorization",
        "credential",
        "private_key",
        "access_key",
        "session_key",
        "cookie",
        "restic_password",
        "board_token",
    ];
    SENSITIVE.iter().any(|needle| key.contains(needle))
}

fn looks_like_secret_token(word: &str) -> bool {
    let clean =
        word.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | ')' | '(' | '[' | ']'));
    const PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "github_pat_",
        "sk-",
        "sk_live_",
        "sk_test_",
        "xoxb-",
        "xoxp-",
        "xapp-",
        "AIza",
        "AKIA",
        "ASIA",
        "hvs.",
        "hvb.",
    ];
    if PREFIXES
        .iter()
        .any(|p| clean.starts_with(p) && clean.len() >= p.len() + 8)
    {
        return true;
    }
    // JWT (`eyJ... . eyJ... . ...`)
    if clean.starts_with("eyJ") {
        let parts: Vec<&str> = clean.split('.').collect();
        if parts.len() == 3 && parts.iter().all(|p| p.len() >= 8) {
            return true;
        }
    }
    false
}

fn redact_url_credentials(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find("://") {
        out.push_str(&rest[..idx + 3]);
        rest = &rest[idx + 3..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>'))
            .unwrap_or(rest.len());
        let url_body = &rest[..end];
        let authority_end = url_body.find('/').unwrap_or(url_body.len());
        let authority = &url_body[..authority_end];
        if let Some(at_pos) = authority.rfind('@') {
            out.push_str("<redacted>@");
            out.push_str(&url_body[at_pos + 1..]);
        } else {
            out.push_str(url_body);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn redact_json_secret_fields(input: &str) -> String {
    let mut out = input.to_owned();
    // ASCII lowercasing is 1:1 on byte length, so byte offsets match `out` even around
    // multi-byte UTF-8 characters whose Unicode lowercase changes byte length (e.g. 'İ', 'K').
    let mut lower = out.to_ascii_lowercase();
    for key in [
        "token",
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "secret",
        "password",
        "webhook_secret",
        "authorization",
    ] {
        let pattern = format!("\"{key}\"");
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(&pattern) {
            let pos = search_from + rel + pattern.len();
            let after_key = &out[pos..];
            let trimmed_ws = after_key.trim_start();
            let ws_len = after_key.len() - trimmed_ws.len();
            if let Some(after_colon) = trimmed_ws.strip_prefix(':') {
                let after_colon_trim = after_colon.trim_start();
                let ws2_len = after_colon.len() - after_colon_trim.len();
                if let Some(val_rest) = after_colon_trim.strip_prefix('"')
                    && let Some(end_quote) = find_unescaped_quote(val_rest)
                {
                    let val_start = pos + ws_len + 1 + ws2_len + 1;
                    let val_end = val_start + end_quote;
                    out.replace_range(val_start..val_end, "<redacted>");
                    lower.replace_range(val_start..val_end, "<redacted>");
                    search_from = val_start + "<redacted>".len();
                    continue;
                }
            }
            search_from = pos;
        }
    }
    out
}

fn find_unescaped_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(idx);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Host resources, uptime, versions, and Tailscale
// ---------------------------------------------------------------------------

fn run_cmd(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn collect_versions(probes: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    if crate::COMMIT != "unknown" {
        versions.insert("board".to_owned(), crate::COMMIT.to_owned());
    }
    for (comp, argv) in probes {
        let Some((bin, args)) = argv.split_first() else {
            continue;
        };
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Some(raw) = run_cmd(bin, &arg_refs) {
            let line = raw
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>();
            if !line.is_empty() {
                versions.insert(comp.clone(), line);
            }
        }
    }
    versions
}

fn collect_uptime_secs() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/uptime").ok()?;
        let secs: f64 = raw.split_whitespace().next()?.parse().ok()?;
        (secs.is_finite() && secs >= 0.0).then_some(secs as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let raw = run_cmd("/usr/sbin/sysctl", &["-n", "kern.boottime"])?;
        parse_macos_boottime(&raw, Utc::now().timestamp())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_macos_boottime(raw: &str, now_secs: i64) -> Option<u64> {
    let after = raw.split("sec =").nth(1)?;
    let boot_secs: i64 = after.split(',').next()?.trim().parse().ok()?;
    (now_secs >= boot_secs).then_some((now_secs - boot_secs) as u64)
}

fn collect_host_resources(cpu_cap_slice: Option<&str>) -> Option<HostResources> {
    let mut res = HostResources::default();
    if let Some((used, total, free)) = collect_root_disk_gib() {
        res.disk_used_gib = Some(used);
        res.disk_total_gib = Some(total);
        res.disk_free_gib = Some(free);
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
            let parts: Vec<f64> = loadavg
                .split_whitespace()
                .take(3)
                .filter_map(|p| p.parse::<f64>().ok())
                .collect();
            if parts.len() == 3 {
                res.load_1m = Some(parts[0]);
                res.load_5m = Some(parts[1]);
                res.load_15m = Some(parts[2]);
                if let Some(cores) = crate::daemon::device::host_cpu_cores()
                    && cores > 0
                {
                    res.cpu_pct = Some(((parts[0] / f64::from(cores)) * 100.0).min(100.0));
                }
            }
        }
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo")
            && let Some((used, total)) = parse_linux_meminfo_gib(&meminfo)
        {
            res.mem_used_gib = Some(used);
            res.mem_total_gib = Some(total);
        }
        if let Some(slice) = cpu_cap_slice
            && let Some(raw) = run_cmd(
                "systemctl",
                &["show", slice, "--property=CPUQuotaPerSecUSec"],
            )
            && let Some(cap) = parse_systemd_cpu_quota(&raw)
        {
            res.slice_cpu_cap_pct = Some(cap);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = cpu_cap_slice;
        if let Some(raw) = run_cmd("/usr/sbin/sysctl", &["-n", "vm.loadavg"]) {
            let nums: Vec<f64> = raw
                .trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
                .split_whitespace()
                .filter_map(|p| p.parse::<f64>().ok())
                .collect();
            if nums.len() >= 3 {
                res.load_1m = Some(nums[0]);
                res.load_5m = Some(nums[1]);
                res.load_15m = Some(nums[2]);
                if let Some(cores) = crate::daemon::device::host_cpu_cores()
                    && cores > 0
                {
                    res.cpu_pct = Some(((nums[0] / f64::from(cores)) * 100.0).min(100.0));
                }
            }
        }
        if let Some(total_bytes) = crate::daemon::device::host_memory() {
            let total_gib = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            res.mem_total_gib = Some((total_gib * 10.0).round() / 10.0);
            if let Some(vm_out) = run_cmd("/usr/bin/vm_stat", &[])
                && let Some(used_gib) = parse_macos_vm_stat_used_gib(&vm_out)
            {
                res.mem_used_gib = Some((used_gib.min(total_gib) * 10.0).round() / 10.0);
            }
        }
    }

    Some(res)
}

fn collect_root_disk_gib() -> Option<(f64, f64, f64)> {
    let raw = run_cmd("df", &["-kP", "/"])?;
    let (used, total, free) = parse_df_kp_gib(&raw)?;
    // On macOS APFS, `/` is a sealed read-only snapshot whose `Used` excludes the shared
    // Data volume in the same container; `total - free` reflects real container usage.
    #[cfg(target_os = "macos")]
    let used = ((total - free).max(used) * 10.0).round() / 10.0;
    Some((used, total, free))
}

/// Parses `df -kP /` output into `(used_gib, total_gib, free_gib)`.
pub fn parse_df_kp_gib(raw: &str) -> Option<(f64, f64, f64)> {
    let line = raw.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }
    let total_kib: f64 = cols[1].parse().ok()?;
    let used_kib: f64 = cols[2].parse().ok()?;
    let avail_kib: f64 = cols[3].parse().ok()?;
    let gib = |k: f64| ((k / (1024.0 * 1024.0)) * 10.0).round() / 10.0;
    Some((gib(used_kib), gib(total_kib), gib(avail_kib)))
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn parse_linux_meminfo_gib(meminfo: &str) -> Option<(f64, f64)> {
    let mut total_kib: Option<f64> = None;
    let mut avail_kib: Option<f64> = None;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kib = rest.split_whitespace().next()?.parse().ok();
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kib = rest.split_whitespace().next()?.parse().ok();
        }
    }
    let total = total_kib?;
    let avail = avail_kib.unwrap_or(0.0);
    let used = (total - avail).max(0.0);
    let gib = |k: f64| ((k / (1024.0 * 1024.0)) * 10.0).round() / 10.0;
    Some((gib(used), gib(total)))
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn parse_systemd_cpu_quota(raw: &str) -> Option<f64> {
    let val = raw.trim().strip_prefix("CPUQuotaPerSecUSec=")?;
    if val == "infinity" || val.is_empty() {
        return None;
    }
    let mut total_secs = 0.0f64;
    let mut parsed_any = false;
    for token in val.split_whitespace() {
        if let Some(ms) = token.strip_suffix("ms") {
            total_secs += ms.parse::<f64>().ok()? / 1_000.0;
            parsed_any = true;
        } else if let Some(us) = token.strip_suffix("us") {
            total_secs += us.parse::<f64>().ok()? / 1_000_000.0;
            parsed_any = true;
        } else {
            let s = token.strip_suffix('s')?;
            total_secs += s.parse::<f64>().ok()?;
            parsed_any = true;
        }
    }
    (parsed_any && total_secs > 0.0).then(|| (total_secs * 100.0 * 10.0).round() / 10.0)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_macos_vm_stat_used_gib(raw: &str) -> Option<f64> {
    let mut page_size: u64 = 16_384;
    if let Some(first) = raw.lines().next()
        && let Some(after) = first.split("page size of ").nth(1)
        && let Some(num) = after.split_whitespace().next()
        && let Ok(parsed) = num.parse::<u64>()
    {
        page_size = parsed;
    }
    let mut active = 0u64;
    let mut wired = 0u64;
    let mut compressed = 0u64;
    for line in raw.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let pages: u64 = v.trim().trim_end_matches('.').parse().unwrap_or(0);
        match k.trim() {
            "Pages active" => active = pages,
            "Pages wired down" => wired = pages,
            "Pages occupied by compressor" => compressed = pages,
            _ => {}
        }
    }
    let used_bytes = active
        .saturating_add(wired)
        .saturating_add(compressed)
        .saturating_mul(page_size);
    (used_bytes > 0).then(|| used_bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn collect_tailscale_status() -> Option<HostNetworkStatus> {
    let raw = run_cmd("tailscale", &["status", "--json"]).or_else(|| {
        run_cmd(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            &["status", "--json"],
        )
    })?;
    parse_tailscale_status_json(&raw)
}

pub fn parse_tailscale_status_json(raw: &str) -> Option<HostNetworkStatus> {
    let val: serde_json::Value = serde_json::from_str(raw).ok()?;
    let self_node = val.get("Self")?;
    let online = self_node.get("Online").and_then(|v| v.as_bool());
    let mode = if online == Some(true) {
        // Inspect active peers: a non-empty `CurAddr` indicates a direct UDP path; an active
        // peer with empty `CurAddr` and non-empty `Relay` is going through DERP.
        let mut any_direct = false;
        let mut any_relayed = false;
        if let Some(peers) = val.get("Peer").and_then(|p| p.as_object()) {
            for peer in peers.values() {
                if peer.get("Active").and_then(|v| v.as_bool()) != Some(true) {
                    continue;
                }
                let has_direct = peer
                    .get("CurAddr")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty());
                if has_direct {
                    any_direct = true;
                } else if peer
                    .get("Relay")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
                {
                    any_relayed = true;
                }
            }
        }
        Some(
            if any_direct {
                "direct"
            } else if any_relayed {
                "relay"
            } else {
                "online"
            }
            .to_owned(),
        )
    } else {
        None
    };
    Some(HostNetworkStatus {
        tailscale_online: online,
        tailscale_mode: mode,
        ..HostNetworkStatus::default()
    })
}

// ---------------------------------------------------------------------------
// Services & Docker containers
// ---------------------------------------------------------------------------

fn collect_services(tel: &TelemetryConfig) -> Vec<ServiceStatus> {
    let mut out = Vec::new();
    if !tel.systemd_services.is_empty() {
        out.extend(collect_systemd_services(&tel.systemd_services));
    }
    if !tel.launchd_services.is_empty() {
        out.extend(collect_launchd_services(&tel.launchd_services));
    }
    out
}

fn collect_docker_containers() -> Vec<ContainerStatus> {
    let Some(raw) = run_cmd("docker", &["ps", "-a", "--format", "{{json .}}"]) else {
        return Vec::new();
    };
    parse_docker_ps_lines(&raw)
}

/// Parses newline-delimited JSON lines from `docker ps -a --format '{{json .}}'`.
pub fn parse_docker_ps_lines(raw: &str) -> Vec<ContainerStatus> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(name) = v.get("Names").and_then(|x| x.as_str()) else {
            continue;
        };
        let state = v
            .get("State")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        let status_text = v.get("Status").and_then(|x| x.as_str()).map(str::to_owned);
        let explicit_health = v
            .get("HealthStatus")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty() && *s != "none")
            .map(str::to_ascii_lowercase);
        let inferred_health = explicit_health.or_else(|| {
            let st = status_text.as_deref()?;
            if st.contains("(unhealthy)") {
                Some("unhealthy".to_owned())
            } else if st.contains("(healthy)") {
                Some("healthy".to_owned())
            } else if st.contains("(health: starting)") {
                Some("starting".to_owned())
            } else {
                None
            }
        });
        out.push(ContainerStatus {
            name: name.to_owned(),
            image: v.get("Image").and_then(|x| x.as_str()).map(str::to_owned),
            state,
            health: inferred_health,
            uptime_secs: None,
            restarts: None,
            status_text,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn collect_systemd_services(units: &[String]) -> Vec<ServiceStatus> {
    let mut args = vec![
        "show".to_owned(),
        "--property=Id,ActiveState,SubState,MainPID,MemoryCurrent,ActiveEnterTimestampUSec,NRestarts"
            .to_owned(),
    ];
    args.extend(units.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some(raw) = run_cmd("systemctl", &arg_refs) else {
        return Vec::new();
    };
    parse_systemd_show_services(&raw, Utc::now())
}

/// Parses batched `systemctl show` output (blank-line-separated property blocks) into services.
pub fn parse_systemd_show_services(raw: &str, now: Timestamp) -> Vec<ServiceStatus> {
    let mut out = Vec::new();
    for block in split_kv_blocks(raw) {
        let Some(name) = block.get("Id") else {
            continue;
        };
        let active = block.get("ActiveState").map(String::as_str).unwrap_or("");
        let sub = block.get("SubState").map(String::as_str).unwrap_or("");
        let state = match active {
            "active" | "reloading" => ServiceState::Running,
            "failed" => ServiceState::Failed,
            "deactivating" | "activating" => ServiceState::Degraded,
            "inactive" => ServiceState::Stopped,
            _ => ServiceState::Unknown,
        };
        let pid = block
            .get("MainPID")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&p| p > 0);
        let memory_mib = block
            .get("MemoryCurrent")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&b| b < u64::MAX / 2)
            .map(|b| ((b as f64 / (1024.0 * 1024.0)) * 10.0).round() / 10.0);
        let uptime_secs = block
            .get("ActiveEnterTimestampUSec")
            .and_then(|v| parse_systemd_usec(v))
            .map(|started| now.signed_duration_since(started).num_seconds().max(0) as u64);
        let restarts = block.get("NRestarts").and_then(|v| v.parse::<u32>().ok());
        out.push(ServiceStatus {
            name: name.clone(),
            kind: "systemd".to_owned(),
            state,
            pid,
            memory_mib,
            uptime_secs,
            restarts,
            detail: (!sub.is_empty()).then(|| format!("{active} ({sub})")),
        });
    }
    out
}

fn collect_launchd_services(labels: &[String]) -> Vec<ServiceStatus> {
    let uid = current_uid_string();
    let mut out = Vec::new();
    for label in labels {
        let target = format!("gui/{uid}/{label}");
        match run_cmd("/bin/launchctl", &["print", &target]) {
            Some(raw) => {
                let parsed = parse_launchctl_print(&raw);
                let state = if parsed.state.as_deref() == Some("running")
                    || (parsed.pid.is_some() && !parsed.last_exit_code.is_some_and(|c| c != 0))
                {
                    ServiceState::Running
                } else if parsed.last_exit_code.is_some_and(|c| c != 0) {
                    ServiceState::Failed
                } else {
                    ServiceState::Stopped
                };
                out.push(ServiceStatus {
                    name: label.clone(),
                    kind: "launchd".to_owned(),
                    state,
                    pid: parsed.pid,
                    memory_mib: None,
                    uptime_secs: None,
                    restarts: parsed
                        .runs
                        .and_then(|r| u32::try_from(r.saturating_sub(1)).ok()),
                    detail: parsed.state,
                });
            }
            None => {
                out.push(ServiceStatus {
                    name: label.clone(),
                    kind: "launchd".to_owned(),
                    state: ServiceState::Stopped,
                    pid: None,
                    memory_mib: None,
                    uptime_secs: None,
                    restarts: None,
                    detail: Some("not loaded".to_owned()),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// macOS launchd scheduled jobs collector
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LaunchctlPrintInfo {
    pub state: Option<String>,
    pub runs: Option<u64>,
    pub last_exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
}

/// Parses `launchctl print gui/<uid>/<label>` output into typed status fields.
///
/// Only top-level service properties (single-tab or <=4-space indentation) populate `state`,
/// `runs`, `last_exit_code`, and `pid`, so nested sections such as event channels or Mach
/// endpoints (`\t\tstate = active`) never overwrite the service's own `state = running`.
pub fn parse_launchctl_print(raw: &str) -> LaunchctlPrintInfo {
    let mut info = LaunchctlPrintInfo::default();
    for line in raw.lines() {
        let is_nested = line.starts_with("\t\t") || line.starts_with("        ");
        let trimmed = line.trim();
        if let Some(v) = trimmed.strip_prefix("state = ") {
            if !is_nested && info.state.is_none() {
                info.state = Some(v.trim().to_owned());
            }
        } else if let Some(v) = trimmed.strip_prefix("runs = ") {
            if !is_nested && info.runs.is_none() {
                info.runs = v.trim().parse().ok();
            }
        } else if let Some(v) = trimmed.strip_prefix("last exit code = ") {
            // `last exit code = 0` or `last exit code = (never exited)`
            if !is_nested && info.last_exit_code.is_none() {
                info.last_exit_code = v.trim().parse().ok();
            }
        } else if let Some(v) = trimmed.strip_prefix("pid = ") {
            if !is_nested && info.pid.is_none() {
                info.pid = v.trim().parse().ok();
            }
        } else if let Some(v) = trimmed.strip_prefix("stdout path = ") {
            let p = v.trim();
            if !p.is_empty() && info.stdout_path.is_none() {
                info.stdout_path = Some(p.to_owned());
            }
        } else if let Some(v) = trimmed.strip_prefix("stderr path = ") {
            let p = v.trim();
            if !p.is_empty() && info.stderr_path.is_none() {
                info.stderr_path = Some(p.to_owned());
            }
        }
    }
    info
}

fn current_uid_string() -> String {
    run_cmd("/usr/bin/id", &["-u"])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "501".to_owned())
}

fn collect_launchd_schedules(cfg: &SchedulesConfig, now: Timestamp) -> Vec<ScheduledJobReport> {
    let dir = cfg
        .launchd_dir
        .clone()
        .or_else(|| dirs::home_dir().map(|h| h.join("Library").join("LaunchAgents")));
    let Some(dir) = dir else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let uid = current_uid_string();
    let mut jobs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("plist") {
            continue;
        }
        let Some(json_text) = run_cmd(
            "/usr/bin/plutil",
            &["-convert", "json", "-o", "-", &path.to_string_lossy()],
        ) else {
            continue;
        };
        let Ok(plist) = serde_json::from_str::<serde_json::Value>(&json_text) else {
            continue;
        };
        let Some(label) = plist.get("Label").and_then(|v| v.as_str()) else {
            continue;
        };
        if !matches_include_exclude(label, &cfg.launchd_include, &cfg.launchd_exclude) {
            continue;
        }
        let target = format!("gui/{uid}/{label}");
        let lc_info =
            run_cmd("/bin/launchctl", &["print", &target]).map(|out| parse_launchctl_print(&out));
        if let Some(job) = build_launchd_job(&plist, lc_info.as_ref(), cfg, now) {
            jobs.push(job);
        }
    }
    jobs.sort_by(|a, b| a.id.cmp(&b.id));
    jobs
}

/// Converts a parsed LaunchAgent plist JSON + optional `launchctl print` info into a
/// [`ScheduledJobReport`]. Returns `None` for pure `KeepAlive` daemons with no schedule.
pub fn build_launchd_job(
    plist: &serde_json::Value,
    lc: Option<&LaunchctlPrintInfo>,
    cfg: &SchedulesConfig,
    now: Timestamp,
) -> Option<ScheduledJobReport> {
    let label = plist.get("Label")?.as_str()?.trim();
    if label.is_empty() {
        return None;
    }
    let start_interval = plist.get("StartInterval").and_then(|v| v.as_u64());
    let calendar = plist.get("StartCalendarInterval");
    let has_watch_paths = plist
        .get("WatchPaths")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());

    // Skip pure KeepAlive daemons that have no timer/calendar schedule.
    if start_interval.is_none() && calendar.is_none() {
        return None;
    }

    let disabled = plist
        .get("Disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || lc.is_none();

    let (schedule_human, schedule_raw, expected_interval_secs) =
        describe_launchd_schedule(start_interval, calendar, has_watch_paths);

    let runs = lc.and_then(|i| i.runs);
    let last_exit_code = lc.and_then(|i| i.last_exit_code);
    let is_running = lc.and_then(|i| i.state.as_deref()) == Some("running");

    let stdout_path = lc
        .and_then(|i| i.stdout_path.as_deref())
        .or_else(|| plist.get("StandardOutPath").and_then(|v| v.as_str()));
    let stderr_path = lc
        .and_then(|i| i.stderr_path.as_deref())
        .or_else(|| plist.get("StandardErrorPath").and_then(|v| v.as_str()));

    // Non-empty log files persist across macOS reboots even after `launchctl print` resets
    // `runs` to 0, proving the job has executed previously.
    let log_mtime = latest_mtime([stderr_path, stdout_path]);

    let state = if disabled {
        ScheduleState::Disabled
    } else if is_running {
        ScheduleState::Running
    } else if last_exit_code.is_some_and(|c| c != 0) {
        ScheduleState::Failing
    } else if runs == Some(0) && log_mtime.is_none() {
        ScheduleState::NeverRun
    } else {
        ScheduleState::Ok
    };

    let last_run_at = log_mtime;

    let next_run_at = if !disabled {
        compute_launchd_next_run(start_interval, calendar, last_run_at, now)
    } else {
        None
    };

    let last_error = if state == ScheduleState::Failing {
        read_failing_log_tail(stderr_path, stdout_path)
    } else {
        None
    };

    let command_preview = extract_launchd_command(plist).map(|cmd| {
        let redacted = redact_secrets(&cmd);
        redacted.chars().take(SCHEDULE_COMMAND_CHARS_MAX).collect()
    });

    let category = classify_category(label, label, &cfg.categories, &cfg.os_timer_prefixes);

    Some(ScheduledJobReport {
        id: label.to_owned(),
        name: label.to_owned(),
        category,
        enabled: !disabled,
        schedule_human,
        schedule_raw,
        expected_interval_secs,
        state,
        last_run_at,
        next_run_at,
        last_duration_ms: None,
        last_exit_code,
        run_count: runs,
        last_error,
        command_preview,
        deep_link: None,
    })
}

fn describe_launchd_schedule(
    interval: Option<u64>,
    calendar: Option<&serde_json::Value>,
    watch_paths: bool,
) -> (String, String, Option<u64>) {
    let mut parts_human = Vec::new();
    let mut parts_raw = Vec::new();
    let mut expected = interval;

    if let Some(secs) = interval {
        parts_human.push(format_interval_human(secs));
        parts_raw.push(format!("StartInterval={secs}"));
    }
    if let Some(cal) = calendar {
        let (h, r, cal_secs) = describe_calendar_interval(cal);
        parts_human.push(h);
        parts_raw.push(r);
        if expected.is_none() {
            expected = cal_secs;
        }
    }
    if watch_paths {
        parts_human.push("+ WatchPaths".to_owned());
    }
    (parts_human.join(" "), parts_raw.join("; "), expected)
}

fn describe_calendar_interval(cal: &serde_json::Value) -> (String, String, Option<u64>) {
    if let Some(arr) = cal.as_array() {
        let raw = format!("StartCalendarInterval={cal}");
        if arr.len() > 1 {
            return (format!("Calendar ({} slots)", arr.len()), raw, Some(86_400));
        }
        if let Some(first) = arr.first() {
            return describe_calendar_dict(first);
        }
    }
    describe_calendar_dict(cal)
}

fn describe_calendar_dict(obj: &serde_json::Value) -> (String, String, Option<u64>) {
    let month = obj.get("Month").and_then(|v| v.as_u64());
    let day = obj.get("Day").and_then(|v| v.as_u64());
    let weekday = obj.get("Weekday").and_then(|v| v.as_u64());
    let hour = obj.get("Hour").and_then(|v| v.as_u64());
    let minute = obj.get("Minute").and_then(|v| v.as_u64()).unwrap_or(0);
    let raw = format!("StartCalendarInterval={obj}");
    if month.is_some() || day.is_some() {
        let time_str = match hour {
            Some(h) => format!("{h:02}:{minute:02}"),
            None => format!(":{minute:02}"),
        };
        let label = match (month, day) {
            (Some(m), Some(d)) => format!("Yearly {m:02}-{d:02} {time_str}"),
            (Some(m), None) => format!("Month {m:02} {time_str}"),
            (None, Some(d)) => format!("Monthly day {d} {time_str}"),
            (None, None) => unreachable!(),
        };
        return (label, raw, None);
    }
    match (weekday, hour) {
        (Some(wd), Some(h)) => (
            format!("{} {:02}:{:02}", weekday_name(wd), h, minute),
            raw,
            Some(604_800),
        ),
        (None, Some(h)) => (format!("Daily {:02}:{:02}", h, minute), raw, Some(86_400)),
        (None, None) => (format!("Hourly :{minute:02}"), raw, Some(3_600)),
        (Some(wd), None) => (
            format!("{} hourly :{minute:02}", weekday_name(wd)),
            raw,
            Some(604_800),
        ),
    }
}

fn weekday_name(wd: u64) -> &'static str {
    match wd % 7 {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        _ => "Sat",
    }
}

fn compute_launchd_next_run(
    interval: Option<u64>,
    calendar: Option<&serde_json::Value>,
    last_run_at: Option<Timestamp>,
    now: Timestamp,
) -> Option<Timestamp> {
    let mut earliest: Option<Timestamp> = None;
    if let Some(cal) = calendar {
        let slots: Vec<&serde_json::Map<String, serde_json::Value>> =
            if let Some(obj) = cal.as_object() {
                vec![obj]
            } else if let Some(arr) = cal.as_array() {
                arr.iter().filter_map(|v| v.as_object()).collect()
            } else {
                Vec::new()
            };
        let local_now = now.with_timezone(&Local);
        for obj in slots {
            let minute = obj.get("Minute").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let hour = obj.get("Hour").and_then(|v| v.as_u64()).map(|v| v as u32);
            let day = obj.get("Day").and_then(|v| v.as_u64()).map(|v| v as u32);
            let month = obj.get("Month").and_then(|v| v.as_u64()).map(|v| v as u32);
            let weekday = obj
                .get("Weekday")
                .and_then(|v| v.as_u64())
                .map(|v| (v % 7) as u32);
            let max_days = if month.is_some() {
                366
            } else if day.is_some() {
                62
            } else {
                8
            };
            for day_offset in 0..max_days {
                let candidate_date = local_now.date_naive() + TimeDelta::days(day_offset);
                if let Some(m) = month
                    && candidate_date.month() != m
                {
                    continue;
                }
                if let Some(d) = day
                    && candidate_date.day() != d
                {
                    continue;
                }
                if let Some(wd) = weekday
                    && candidate_date.weekday().num_days_from_sunday() != wd
                {
                    continue;
                }
                let h = match hour {
                    Some(h) => h,
                    None if day_offset == 0 => {
                        let cur_h = local_now.hour();
                        if local_now.minute() < minute {
                            cur_h
                        } else if cur_h < 23 {
                            cur_h + 1
                        } else {
                            continue;
                        }
                    }
                    None => 0,
                };
                if let Some(naive_dt) = candidate_date.and_hms_opt(h, minute, 0)
                    && let Some(local_dt) = Local.from_local_datetime(&naive_dt).earliest()
                {
                    let utc_dt = local_dt.with_timezone(&Utc);
                    if utc_dt > now {
                        if earliest.is_none_or(|prev| utc_dt < prev) {
                            earliest = Some(utc_dt);
                        }
                        break;
                    }
                }
            }
        }
    }
    // `last_run_at` for launchd comes from stdout/stderr log mtime, which stops updating when a
    // clean run prints nothing. Only derive a `next_run_at` from `last_run_at + StartInterval`
    // when it lands in the future; otherwise leave `None` so the server tracks real cadence via
    // `launchctl print`'s `runs` counter instead of fabricating a past due time.
    if let (Some(secs), Some(last)) = (interval, last_run_at) {
        let candidate = last + TimeDelta::seconds(secs as i64);
        if candidate > now && earliest.is_none_or(|prev| candidate < prev) {
            earliest = Some(candidate);
        }
    }
    earliest
}

fn latest_mtime(paths: [Option<&str>; 2]) -> Option<Timestamp> {
    paths
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            // Ignore empty log files: launchd creates 0-byte stdout/stderr files at load time,
            // so their mtime reflects creation rather than a job run.
            if meta.len() == 0 {
                return None;
            }
            meta.modified().ok()
        })
        .map(DateTime::<Utc>::from)
        .max()
}

/// Reads at most the final 64 KiB of a failing job's log so large log files never cause
/// memory or CPU spikes during periodic host reporting.
fn read_tail_bytes(path: &str, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn read_failing_log_tail(stderr_path: Option<&str>, stdout_path: Option<&str>) -> Option<String> {
    const MAX_LOG_READ_BYTES: u64 = 64 * 1024;
    for path in [stderr_path, stdout_path].into_iter().flatten() {
        if let Some(content) = read_tail_bytes(path, MAX_LOG_READ_BYTES)
            && let Some(tail) = redact_tail(&content, 15, SCHEDULE_ERROR_BYTES_MAX)
        {
            return Some(tail);
        }
    }
    None
}

fn extract_launchd_command(plist: &serde_json::Value) -> Option<String> {
    if let Some(arr) = plist.get("ProgramArguments").and_then(|v| v.as_array()) {
        let joined: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        if !joined.is_empty() {
            return Some(joined.join(" "));
        }
    }
    plist
        .get("Program")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// T3 Code read-only `statev2.sqlite` (`scheduled_tasks`) collector
// ---------------------------------------------------------------------------

/// Reads `scheduled_tasks` from a T3 Code `statev2.sqlite` database strictly read-only.
pub fn collect_t3_schedules(
    db_path: &Path,
    cfg: &SchedulesConfig,
) -> Result<Vec<ScheduledJobReport>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening T3 state DB read-only at {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update(None, "query_only", "ON")?;

    let mut stmt = conn.prepare(
        "SELECT task_id, title, prompt, enabled, schedule_json, thread_id, \
                next_run_at, last_run_at, last_run_status, last_run_error, run_count \
         FROM scheduled_tasks ORDER BY title",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)? != 0,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, Option<String>>(9)?,
            r.get::<_, i64>(10)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (
            task_id,
            title,
            prompt,
            enabled,
            schedule_json,
            thread_id,
            next_run_raw,
            last_run_raw,
            last_status,
            last_error,
            run_count,
        ) = row?;

        let (schedule_human, expected_interval_secs) = describe_t3_schedule(&schedule_json);
        let state = if !enabled {
            ScheduleState::Disabled
        } else {
            match last_status.to_ascii_lowercase().as_str() {
                "running" | "in_progress" => ScheduleState::Running,
                "failed" | "error" => ScheduleState::Failing,
                "never" | "" => ScheduleState::NeverRun,
                _ => ScheduleState::Ok,
            }
        };

        let last_run_at = last_run_raw
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let next_run_at = next_run_raw
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let deep_link = thread_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|tid| match cfg.t3_link_base.as_deref() {
                Some(base) => format!("{}/{tid}", base.trim_end_matches('/')),
                None => format!("t3://thread/{tid}"),
            });

        let redacted_prompt: String = redact_secrets(&prompt)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(SCHEDULE_COMMAND_CHARS_MAX)
            .collect();

        let redacted_err = last_error
            .as_deref()
            .and_then(|e| redact_tail(e, 15, SCHEDULE_ERROR_BYTES_MAX));

        let category = classify_category(&task_id, &title, &cfg.categories, &cfg.os_timer_prefixes);
        let category = if category == ScheduleCategory::Other {
            ScheduleCategory::AgentOps
        } else {
            category
        };

        out.push(ScheduledJobReport {
            id: task_id,
            name: title,
            category,
            enabled,
            schedule_human,
            schedule_raw: schedule_json.chars().take(500).collect(),
            expected_interval_secs,
            state,
            last_run_at,
            next_run_at,
            last_duration_ms: None,
            last_exit_code: (state == ScheduleState::Failing).then_some(1),
            run_count: Some(run_count.max(0) as u64),
            last_error: redacted_err,
            command_preview: (!redacted_prompt.is_empty()).then_some(redacted_prompt),
            deep_link,
        });
    }
    Ok(out)
}

/// Normalizes T3's `schedule_json` into `(schedule_human, expected_interval_secs)`.
pub fn describe_t3_schedule(schedule_json: &str) -> (String, Option<u64>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(schedule_json) else {
        return ("Scheduled".to_owned(), None);
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("interval") => {
            let secs = v
                .get("everyMs")
                .and_then(|n| n.as_u64())
                .map(|ms| (ms / 1_000).max(1));
            match secs {
                Some(s) => (format_interval_human(s), Some(s)),
                None => ("Interval".to_owned(), None),
            }
        }
        Some("fixed_time") => {
            let tod = v
                .get("timeOfDay")
                .and_then(|s| s.as_str())
                .unwrap_or("00:00");
            let tz = v
                .get("timezone")
                .and_then(|s| s.as_str())
                .map(|t| match t {
                    "Asia/Tokyo" => " JST",
                    "UTC" => " UTC",
                    _ => "",
                })
                .unwrap_or("");
            if let Some(days) = v
                .get("weekdays")
                .or_else(|| v.get("daysOfWeek"))
                .and_then(|d| d.as_array())
                .filter(|a| !a.is_empty() && a.len() < 7)
            {
                let labels: Vec<&str> = days
                    .iter()
                    .filter_map(|d| d.as_u64().map(weekday_name))
                    .collect();
                (format!("{} {tod}{tz}", labels.join(",")), Some(604_800))
            } else {
                (format!("Daily {tod}{tz}"), Some(86_400))
            }
        }
        Some("once") => {
            let run_at = v.get("runAt").and_then(|s| s.as_str()).unwrap_or("");
            let short = run_at.get(..16).unwrap_or(run_at);
            (format!("Once {short}"), None)
        }
        _ => ("Scheduled".to_owned(), None),
    }
}

// ---------------------------------------------------------------------------
// Linux systemd timers & crontab collectors
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SystemdTimerJson {
    unit: String,
    #[serde(default)]
    activates: Option<String>,
    #[serde(default)]
    next: Option<u64>,
    #[serde(default)]
    last: Option<u64>,
}

fn collect_systemd_timers(cfg: &SchedulesConfig) -> Vec<ScheduledJobReport> {
    let Some(raw) = run_cmd(
        "systemctl",
        &["list-timers", "--all", "--output=json", "--no-pager"],
    ) else {
        return Vec::new();
    };
    let Ok(timers) = serde_json::from_str::<Vec<SystemdTimerJson>>(&raw) else {
        return Vec::new();
    };
    if timers.is_empty() {
        return Vec::new();
    }

    // Batch `systemctl show` across all timer + service units in one call.
    let mut show_args = vec![
        "show".to_owned(),
        "--property=Id,Description,ActiveState,SubState,Result,ExecMainStatus,ExecMainStartTimestampUSec,ExecMainExitTimestampUSec,UnitFileState,TimersCalendar,TimersMonotonic".to_owned(),
    ];
    for t in &timers {
        show_args.push(t.unit.clone());
        if let Some(svc) = &t.activates {
            show_args.push(svc.clone());
        }
    }
    let arg_refs: Vec<&str> = show_args.iter().map(String::as_str).collect();
    let show_raw = run_cmd("systemctl", &arg_refs).unwrap_or_default();
    let mut jobs = parse_systemd_timers_with_show(&raw, &show_raw, cfg);

    // Populate redacted journal tails only for failing units.
    for job in &mut jobs {
        if job.state == ScheduleState::Failing {
            let svc_unit = job.id.replace(".timer", ".service");
            if let Some(log) = run_cmd(
                "journalctl",
                &["-u", &svc_unit, "-n", "20", "--no-pager", "-o", "cat"],
            ) {
                job.last_error = redact_tail(&log, 15, SCHEDULE_ERROR_BYTES_MAX);
            }
        }
    }
    jobs
}

/// Pure parser combining `systemctl list-timers --all --output=json` and `systemctl show`.
pub fn parse_systemd_timers_with_show(
    timers_json: &str,
    show_output: &str,
    cfg: &SchedulesConfig,
) -> Vec<ScheduledJobReport> {
    let Ok(timers) = serde_json::from_str::<Vec<SystemdTimerJson>>(timers_json) else {
        return Vec::new();
    };
    let mut props_by_unit: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for block in split_kv_blocks(show_output) {
        if let Some(id) = block.get("Id").cloned() {
            props_by_unit.insert(id, block);
        }
    }

    let mut out = Vec::with_capacity(timers.len());
    for timer in timers {
        let t_props = props_by_unit.get(&timer.unit);
        let svc_name = timer
            .activates
            .clone()
            .unwrap_or_else(|| timer.unit.replace(".timer", ".service"));
        let s_props = props_by_unit.get(&svc_name);

        let enabled = t_props
            .and_then(|p| p.get("ActiveState"))
            .map(|s| s == "active" || s == "waiting")
            .unwrap_or(true);
        let svc_active = s_props
            .and_then(|p| p.get("ActiveState"))
            .map(String::as_str)
            .unwrap_or("");
        let svc_result = s_props
            .and_then(|p| p.get("Result"))
            .map(String::as_str)
            .unwrap_or("");
        let exit_code = s_props
            .and_then(|p| p.get("ExecMainStatus"))
            .and_then(|v| v.parse::<i32>().ok());

        let last_run_at = timer.last.and_then(usec_to_timestamp);
        let next_run_at = timer.next.and_then(usec_to_timestamp);

        let last_duration_ms = match (
            s_props
                .and_then(|p| p.get("ExecMainStartTimestampUSec"))
                .and_then(|v| parse_systemd_usec(v)),
            s_props
                .and_then(|p| p.get("ExecMainExitTimestampUSec"))
                .and_then(|v| parse_systemd_usec(v)),
        ) {
            (Some(start), Some(end)) if end >= start => {
                Some(end.signed_duration_since(start).num_milliseconds().max(0) as u64)
            }
            _ => None,
        };

        let state = if !enabled {
            ScheduleState::Disabled
        } else if svc_active == "activating" || svc_active == "active" {
            ScheduleState::Running
        } else if svc_active == "failed"
            || (svc_result != "success" && !svc_result.is_empty())
            || exit_code.is_some_and(|c| c != 0)
        {
            ScheduleState::Failing
        } else if last_run_at.is_none() {
            ScheduleState::NeverRun
        } else {
            ScheduleState::Ok
        };

        let cal_spec = t_props
            .and_then(|p| p.get("TimersCalendar"))
            .filter(|s| !s.is_empty())
            .cloned();
        let mono_spec = t_props
            .and_then(|p| p.get("TimersMonotonic"))
            .filter(|s| !s.is_empty())
            .cloned();
        let (schedule_human, schedule_raw, expected_interval_secs) = describe_systemd_timer_spec(
            cal_spec.as_deref(),
            mono_spec.as_deref(),
            last_run_at,
            next_run_at,
        );

        let name = t_props
            .and_then(|p| p.get("Description"))
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| timer.unit.clone());

        let category =
            classify_category(&timer.unit, &name, &cfg.categories, &cfg.os_timer_prefixes);

        out.push(ScheduledJobReport {
            id: timer.unit.clone(),
            name,
            category,
            enabled,
            schedule_human,
            schedule_raw,
            expected_interval_secs,
            state,
            last_run_at,
            next_run_at,
            last_duration_ms,
            last_exit_code: exit_code,
            run_count: None,
            last_error: (state == ScheduleState::Failing && !svc_result.is_empty())
                .then(|| format!("systemd result={svc_result}")),
            command_preview: Some(svc_name),
            deep_link: None,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn describe_systemd_timer_spec(
    cal: Option<&str>,
    mono: Option<&str>,
    last: Option<Timestamp>,
    next: Option<Timestamp>,
) -> (String, String, Option<u64>) {
    let raw = match (cal, mono) {
        (Some(c), _) => c.chars().take(500).collect::<String>(),
        (None, Some(m)) => m.chars().take(500).collect::<String>(),
        (None, None) => String::new(),
    };
    let inferred_secs = match (last, next) {
        (Some(l), Some(n)) if n > l => {
            let diff = n.signed_duration_since(l).num_seconds();
            (diff > 0 && diff <= 31 * 86_400).then_some(diff as u64)
        }
        _ => None,
    };
    if let Some(m) = mono
        && let Some(secs) = extract_systemd_monotonic_secs(m)
    {
        return (format_interval_human(secs), raw, Some(secs));
    }
    if let Some(c) = cal {
        let exprs: Vec<&str> = c
            .split("OnCalendar=")
            .skip(1)
            .filter_map(|s| {
                let expr = s.split([';', '}']).next().map(str::trim).unwrap_or("");
                (!expr.is_empty()).then_some(expr)
            })
            .collect();
        if !exprs.is_empty() {
            return (
                exprs.join(", ").chars().take(120).collect(),
                raw,
                inferred_secs,
            );
        }
    }
    let human = inferred_secs
        .map(format_interval_human)
        .unwrap_or_else(|| "Systemd timer".to_owned());
    (human, raw, inferred_secs)
}

fn extract_systemd_monotonic_secs(spec: &str) -> Option<u64> {
    for key in [
        "OnUnitActiveUSec=",
        "OnUnitActiveSec=",
        "OnUnitInactiveUSec=",
        "OnUnitInactiveSec=",
    ] {
        for after in spec.split(key).skip(1) {
            let token = after.split([';', '}']).next().unwrap_or("").trim();
            if let Some(secs) = parse_systemd_duration_secs(token) {
                return Some(secs);
            }
        }
    }
    None
}

fn parse_systemd_duration_secs(raw: &str) -> Option<u64> {
    let mut total = 0u64;
    for part in raw.split_whitespace() {
        if let Some(n) = part.strip_suffix("min") {
            total += n.parse::<u64>().ok()? * 60;
        } else if let Some(n) = part.strip_suffix('w') {
            total += n.parse::<u64>().ok()? * 604_800;
        } else if let Some(n) = part.strip_suffix('h') {
            total += n.parse::<u64>().ok()? * 3_600;
        } else if let Some(n) = part.strip_suffix('d') {
            total += n.parse::<u64>().ok()? * 86_400;
        } else {
            let n = part.strip_suffix('s')?;
            total += n.parse::<u64>().ok()?;
        }
    }
    (total > 0).then_some(total)
}

fn usec_to_timestamp(usec: u64) -> Option<Timestamp> {
    if usec == 0 || usec > i64::MAX as u64 {
        return None;
    }
    DateTime::from_timestamp_micros(usec as i64)
}

fn parse_systemd_usec(raw: &str) -> Option<Timestamp> {
    let v: u64 = raw.trim().parse().ok()?;
    usec_to_timestamp(v)
}

fn collect_crontab_schedules(cfg: &SchedulesConfig) -> Vec<ScheduledJobReport> {
    let Some(raw) = run_cmd("crontab", &["-l"]) else {
        return Vec::new();
    };
    parse_crontab(&raw, cfg)
}

/// Parses `crontab -l` text into normalized [`ScheduledJobReport`] entries with redacted commands.
pub fn parse_crontab(raw: &str, cfg: &SchedulesConfig) -> Vec<ScheduledJobReport> {
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Skip env var assignments (`PATH=...`, `SHELL=...`).
        if let Some((lhs, _)) = trimmed.split_once('=')
            && !lhs.contains(char::is_whitespace)
            && !trimmed.starts_with('@')
        {
            continue;
        }
        let (schedule, command) = if trimmed.starts_with('@') {
            let Some((sched, cmd)) = trimmed.split_once(char::is_whitespace) else {
                continue;
            };
            (sched.to_owned(), cmd.trim().to_owned())
        } else {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() < 6 {
                continue;
            }
            (parts[..5].join(" "), parts[5..].join(" "))
        };
        let redacted_cmd: String = redact_secrets(&command)
            .chars()
            .take(SCHEDULE_COMMAND_CHARS_MAX)
            .collect();
        let id = format!("cron-line-{}", idx + 1);
        let name: String = redacted_cmd.chars().take(80).collect();
        let category = classify_category(&id, &name, &cfg.categories, &cfg.os_timer_prefixes);
        out.push(ScheduledJobReport {
            id,
            name,
            category,
            enabled: true,
            schedule_human: schedule.clone(),
            schedule_raw: schedule,
            expected_interval_secs: None,
            state: ScheduleState::Ok,
            last_run_at: None,
            next_run_at: None,
            last_duration_ms: None,
            last_exit_code: None,
            run_count: None,
            last_error: None,
            command_preview: Some(redacted_cmd),
            deep_link: None,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Shared classification & formatting helpers
// ---------------------------------------------------------------------------

fn split_kv_blocks(raw: &str) -> Vec<BTreeMap<String, String>> {
    let mut blocks = Vec::new();
    let mut cur: BTreeMap<String, String> = BTreeMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            if matches!(k, "TimersMonotonic" | "TimersCalendar") {
                if !v.is_empty() {
                    cur.entry(k.to_owned())
                        .and_modify(|existing| {
                            if !existing.is_empty() {
                                existing.push(' ');
                            }
                            existing.push_str(v);
                        })
                        .or_insert_with(|| v.to_owned());
                }
            } else {
                cur.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    blocks
}

fn matches_include_exclude(label: &str, include: &[String], exclude: &[String]) -> bool {
    if !include.is_empty() && !include.iter().any(|p| prefix_or_glob_match(p, label)) {
        return false;
    }
    if exclude.iter().any(|p| prefix_or_glob_match(p, label)) {
        return false;
    }
    true
}

fn prefix_or_glob_match(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        value.starts_with(prefix)
    } else {
        value == pattern || value.starts_with(pattern)
    }
}

fn classify_category(
    id: &str,
    name: &str,
    overrides: &BTreeMap<String, ScheduleCategory>,
    extra_os_prefixes: &[String],
) -> ScheduleCategory {
    let id_lower = id.to_ascii_lowercase();
    let name_lower = name.to_ascii_lowercase();
    for (needle, cat) in overrides {
        let n = needle.to_ascii_lowercase();
        if id_lower.contains(&n) || name_lower.contains(&n) {
            return *cat;
        }
    }
    if DEFAULT_OS_TIMER_PREFIXES
        .iter()
        .any(|p| id_lower.starts_with(p))
        || extra_os_prefixes
            .iter()
            .any(|p| id_lower.starts_with(&p.to_ascii_lowercase()))
    {
        return ScheduleCategory::Os;
    }
    let combined = format!("{id_lower} {name_lower}");
    if combined.contains("backup")
        || combined.contains("snapshot")
        || combined.contains("restic")
        || combined.contains("replicate")
        || combined.contains("prune")
        || combined.contains("archive")
    {
        ScheduleCategory::Backup
    } else if combined.contains("cert")
        || combined.contains("vault")
        || combined.contains("egress")
        || combined.contains("security")
        || combined.contains("audit")
    {
        ScheduleCategory::Security
    } else if combined.contains("monitor")
        || combined.contains("heartbeat")
        || combined.contains("alert")
        || combined.contains("health")
        || combined.contains("disk-space")
        || combined.contains("storage-check")
    {
        ScheduleCategory::Monitoring
    } else if combined.contains("sync")
        || combined.contains("export")
        || combined.contains("upstream")
    {
        ScheduleCategory::Sync
    } else if combined.contains("dependabot")
        || combined.contains("cache")
        || combined.contains("clean")
        || combined.contains("gc")
    {
        ScheduleCategory::Maintenance
    } else if combined.contains("factory")
        || combined.contains("skills")
        || combined.contains("settle")
        || combined.contains("friction")
        || combined.contains("review")
    {
        ScheduleCategory::AgentOps
    } else {
        ScheduleCategory::Other
    }
}

fn format_interval_human(secs: u64) -> String {
    if secs >= 604_800 && secs.is_multiple_of(604_800) {
        let w = secs / 604_800;
        if w == 1 {
            "Weekly".to_owned()
        } else {
            format!("Every {w}w")
        }
    } else if secs >= 86_400 && secs.is_multiple_of(86_400) {
        let d = secs / 86_400;
        if d == 1 {
            "Daily".to_owned()
        } else {
            format!("Every {d}d")
        }
    } else if secs >= 3_600 && secs.is_multiple_of(3_600) {
        let h = secs / 3_600;
        if h == 1 {
            "Hourly".to_owned()
        } else {
            format!("Every {h}h")
        }
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("Every {}m", secs / 60)
    } else {
        format!("Every {secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_redaction_strips_tokens_urls_env_json_and_caps_tail() {
        let raw = "\x1b[31mERROR\x1b[0m failed calling https://alice:supersecret123@api.example.com/v1\n\
                   Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signaturepart\n\
                   BOARD_TOKEN=abcdef0123456789 RESTIC_PASSWORD=my-vault-pass\n\
                   ghp_1234567890abcdef1234567890abcdef1234 sk-live-secret1234567890\n\
                   {\"api_key\": \"secret-val-99\", \"ok\": false}\n\
                   --token mysecretcliarg";
        let clean = redact_secrets(raw);
        for leaked in [
            "supersecret123",
            "eyJhbGci",
            "abcdef0123456789",
            "my-vault-pass",
            "ghp_123456",
            "sk-live-secret",
            "secret-val-99",
            "mysecretcliarg",
            "\x1b[31m",
        ] {
            assert!(!clean.contains(leaked), "leaked {leaked:?} in:\n{clean}");
        }
        assert!(
            clean.contains("https://<redacted>@api.example.com/v1"),
            "{clean}"
        );
        assert!(clean.contains("BOARD_TOKEN=<redacted>"), "{clean}");
        assert!(clean.contains("\"api_key\": \"<redacted>\""), "{clean}");

        let huge = format!("{}\nFINAL_FAILURE_LINE", "x".repeat(4000));
        let tail = redact_tail(&huge, 15, SCHEDULE_ERROR_BYTES_MAX).unwrap();
        assert!(tail.len() <= SCHEDULE_ERROR_BYTES_MAX);
        assert!(tail.ends_with("FINAL_FAILURE_LINE"));
    }

    #[test]
    fn secret_redaction_splits_tab_separated_flags_and_tokens() {
        let raw = "cmd\t--token\tghp_1234567890abcdef1234\n\
                   cmd\t--secret\topaque-cli-secret\n\
                   cmd\t--api-key\t\tmulti-tab-secret\n\
                   Authorization:\tBearer\topaque-tab-bearer\n\
                   Bearer\tstandalone-tab-bearer\n\
                   X-API-Key:\ttab-header-key\n\
                   prefix\tghp_abcdef1234567890abcd\tsk-abcdef1234567890abcd\tsuffix";
        let clean = redact_secrets(raw);
        for leaked in [
            "ghp_1234567890abcdef1234",
            "opaque-cli-secret",
            "multi-tab-secret",
            "opaque-tab-bearer",
            "standalone-tab-bearer",
            "tab-header-key",
            "ghp_abcdef1234567890abcd",
            "sk-abcdef1234567890abcd",
        ] {
            assert!(!clean.contains(leaked), "leaked {leaked:?} in:\n{clean}");
        }
        assert!(clean.contains("cmd --token <redacted>"), "{clean}");
        assert!(clean.contains("cmd --secret <redacted>"), "{clean}");
        assert!(clean.contains("cmd --api-key <redacted>"), "{clean}");
        assert!(
            clean.contains("Authorization: Bearer <redacted>"),
            "{clean}"
        );
        assert!(clean.contains("Bearer <redacted>"), "{clean}");
        assert!(clean.contains("X-API-Key: <redacted>"), "{clean}");
        assert!(
            clean.contains("prefix <redacted> <redacted> suffix"),
            "{clean}"
        );
    }

    #[test]
    fn secret_redaction_matches_json_secret_keys_case_insensitively_with_utf8() {
        // Includes 'İ' (U+0130, 2 -> 3 bytes under Unicode to_lowercase) and 'K' (U+212A, 3 -> 1 byte)
        // plus CJK and 4-byte emoji to verify ASCII lowercasing preserves UTF-8 byte offsets.
        let raw = "{\"note\": \"İK🔑秘密\", \"API_KEY\": \"İ-secret-🔑\", \"ApiKey\": \"camel-secret\", \
                   \"Token\": \"秘密-tok\", \"TOKEN\": \"tok-two\", \"Authorization\": \"Bearer auth-secret\", \
                   \"Password\": \"pw-K-123\", \"ACCESS_TOKEN\": \"acc-secret\", \"Refresh_Token\": \"ref-secret\", \
                   \"SECRET\": \"top-secret\", \"Webhook_Secret\": \"wh-secret\", \"tail\": \"İK日本語\"}\n\
                   {\"label\": \"Token\", \"Token\": \"actual-secret\"}";
        let clean = redact_secrets(raw);
        for leaked in [
            "İ-secret-🔑",
            "camel-secret",
            "秘密-tok",
            "tok-two",
            "auth-secret",
            "pw-K-123",
            "acc-secret",
            "ref-secret",
            "top-secret",
            "wh-secret",
            "actual-secret",
        ] {
            assert!(!clean.contains(leaked), "leaked {leaked:?} in:\n{clean}");
        }
        for expected in [
            "\"note\": \"İK🔑秘密\"",
            "\"API_KEY\": \"<redacted>\"",
            "\"ApiKey\": \"<redacted>\"",
            "\"Token\": \"<redacted>\"",
            "\"TOKEN\": \"<redacted>\"",
            "\"Authorization\": \"<redacted>\"",
            "\"Password\": \"<redacted>\"",
            "\"ACCESS_TOKEN\": \"<redacted>\"",
            "\"Refresh_Token\": \"<redacted>\"",
            "\"SECRET\": \"<redacted>\"",
            "\"Webhook_Secret\": \"<redacted>\"",
            "\"tail\": \"İK日本語\"",
            "\"label\": \"Token\"",
        ] {
            assert!(
                clean.contains(expected),
                "missing {expected:?} in:\n{clean}"
            );
        }
    }

    #[test]
    fn launchd_plist_and_launchctl_print_parse_cleanly() {
        let plist = serde_json::json!({
            "Label": "com.example.weekly-audit",
            "ProgramArguments": ["/usr/bin/env", "bun", "/opt/audit.js", "--token", "secret123"],
            "StartCalendarInterval": { "Weekday": 1, "Hour": 13, "Minute": 30 }
        });
        let lc_raw = "\
gui/501/com.example.weekly-audit = {
\tstate = not running
\truns = 3
\tlast exit code = 1
}";
        let lc = parse_launchctl_print(lc_raw);
        assert_eq!(lc.runs, Some(3));
        assert_eq!(lc.last_exit_code, Some(1));

        let now = Utc.with_ymd_and_hms(2026, 11, 26, 0, 0, 0).unwrap();
        let cfg = SchedulesConfig::default();
        let job = build_launchd_job(&plist, Some(&lc), &cfg, now).unwrap();
        assert_eq!(job.id, "com.example.weekly-audit");
        assert_eq!(job.state, ScheduleState::Failing);
        assert_eq!(job.schedule_human, "Mon 13:30");
        assert_eq!(job.expected_interval_secs, Some(604_800));
        assert_eq!(job.last_exit_code, Some(1));
        assert_eq!(job.run_count, Some(3));
        assert!(
            !job.command_preview
                .as_deref()
                .unwrap()
                .contains("secret123")
        );

        // Pure KeepAlive daemon without schedule returns None.
        let daemon_plist = serde_json::json!({
            "Label": "com.example.daemon",
            "KeepAlive": true
        });
        assert!(build_launchd_job(&daemon_plist, Some(&lc), &cfg, now).is_none());
    }

    #[test]
    fn t3_sqlite_collector_reads_scheduled_tasks_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("statev2.sqlite");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE scheduled_tasks (
                    task_id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    enabled INTEGER NOT NULL,
                    schedule_json TEXT NOT NULL,
                    thread_id TEXT,
                    next_run_at TEXT,
                    last_run_at TEXT,
                    last_run_status TEXT NOT NULL,
                    last_run_error TEXT,
                    run_count INTEGER NOT NULL
                );
                INSERT INTO scheduled_tasks VALUES (
                    'sched-1', 'Daily sync', 'Sync upstream with token=secret42', 1,
                    '{\"type\":\"fixed_time\",\"timeOfDay\":\"10:00\",\"timezone\":\"Asia/Tokyo\"}',
                    'thread-abc', '2026-11-27T01:00:00Z', '2026-11-26T01:00:00Z',
                    'succeeded', NULL, 5
                );
                INSERT INTO scheduled_tasks VALUES (
                    'sched-2', 'Weekly monitor', 'Check compatibility', 1,
                    '{\"type\":\"fixed_time\",\"timeOfDay\":\"10:00\",\"daysOfWeek\":[1],\"timezone\":\"Asia/Tokyo\"}',
                    NULL, '2026-11-30T01:00:00Z', NULL,
                    'never', NULL, 0
                );
                INSERT INTO scheduled_tasks VALUES (
                    'sched-3', 'Disabled job', 'Old task', 0,
                    '{\"type\":\"interval\",\"everyMs\":3600000}',
                    NULL, NULL, NULL,
                    'failed', 'Bearer ghp_1234567890abcdef1234', 2
                );",
            )
            .unwrap();
        }
        let cfg = SchedulesConfig {
            t3_link_base: Some("t3://thread/".to_owned()),
            ..SchedulesConfig::default()
        };
        let jobs = collect_t3_schedules(&db_path, &cfg).unwrap();
        assert_eq!(jobs.len(), 3);
        let sync = jobs.iter().find(|j| j.id == "sched-1").unwrap();
        assert_eq!(sync.state, ScheduleState::Ok);
        assert_eq!(sync.schedule_human, "Daily 10:00 JST");
        assert_eq!(sync.deep_link.as_deref(), Some("t3://thread/thread-abc"));
        assert!(
            !sync
                .command_preview
                .as_deref()
                .unwrap()
                .contains("secret42")
        );
        assert_eq!(sync.category, ScheduleCategory::Sync);

        let weekly = jobs.iter().find(|j| j.id == "sched-2").unwrap();
        assert_eq!(weekly.state, ScheduleState::NeverRun);
        assert_eq!(weekly.schedule_human, "Mon 10:00 JST");
        assert_eq!(weekly.expected_interval_secs, Some(604_800));

        let disabled = jobs.iter().find(|j| j.id == "sched-3").unwrap();
        assert_eq!(disabled.state, ScheduleState::Disabled);
        assert!(
            !disabled
                .last_error
                .as_deref()
                .unwrap()
                .contains("ghp_12345")
        );
    }

    #[test]
    fn linux_systemd_timers_services_docker_and_crontab_parse_cleanly() {
        let timers_json = r#"[
            {"next":1795652062000000,"last":1795652002000000,"unit":"app-heartbeat.timer","activates":"app-heartbeat.service"},
            {"next":1795682665000000,"last":1795611061000000,"unit":"apt-daily.timer","activates":"apt-daily.service"}
        ]"#;
        let show_out = "\
Id=app-heartbeat.timer
Description=App heartbeat timer
ActiveState=active
TimersMonotonic=OnUnitActiveSec=1min

Id=app-heartbeat.service
ActiveState=inactive
Result=success
ExecMainStatus=0
ExecMainStartTimestampUSec=1795652002000000
ExecMainExitTimestampUSec=1795652002500000

Id=apt-daily.timer
Description=Daily apt download activities
ActiveState=active
TimersCalendar=OnCalendar=*-*-* 06:00:00

Id=apt-daily.service
ActiveState=failed
Result=exit-code
ExecMainStatus=1
";
        let cfg = SchedulesConfig::default();
        let jobs = parse_systemd_timers_with_show(timers_json, show_out, &cfg);
        assert_eq!(jobs.len(), 2);
        let hb = jobs.iter().find(|j| j.id == "app-heartbeat.timer").unwrap();
        assert_eq!(hb.state, ScheduleState::Ok);
        assert_eq!(hb.category, ScheduleCategory::Monitoring);
        assert_eq!(hb.schedule_human, "Every 1m");
        assert_eq!(hb.last_duration_ms, Some(500));

        let apt = jobs.iter().find(|j| j.id == "apt-daily.timer").unwrap();
        assert_eq!(apt.category, ScheduleCategory::Os);
        assert_eq!(apt.state, ScheduleState::Failing);
        assert_eq!(apt.last_exit_code, Some(1));

        let docker_raw = "\
{\"Names\":\"app-db\",\"Image\":\"postgres:16\",\"State\":\"running\",\"Status\":\"Up 9 hours (healthy)\",\"HealthStatus\":\"healthy\"}
{\"Names\":\"app-web\",\"Image\":\"nginx:alpine\",\"State\":\"running\",\"Status\":\"Up 2 hours\",\"HealthStatus\":\"none\"}";
        let containers = parse_docker_ps_lines(docker_raw);
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].name, "app-db");
        assert_eq!(containers[0].health.as_deref(), Some("healthy"));
        assert_eq!(containers[1].health, None);

        let cron_raw = "# comment\nPATH=/usr/bin\n*/5 * * * * /usr/local/bin/check --token secret99\n@daily /usr/local/bin/backup\n";
        let cron_jobs = parse_crontab(cron_raw, &cfg);
        assert_eq!(cron_jobs.len(), 2);
        assert_eq!(cron_jobs[0].schedule_human, "*/5 * * * *");
        assert!(
            !cron_jobs[0]
                .command_preview
                .as_deref()
                .unwrap()
                .contains("secret99")
        );
        assert_eq!(cron_jobs[1].category, ScheduleCategory::Backup);
    }

    #[test]
    fn systemd_multiline_timers_monotonic_usec_and_calendar_parse_without_next_run() {
        // Exact multi-line `systemctl show` shape where `OnUnitActiveUSec=1min` is followed by
        // `OnBootUSec=3min` and `next` is transiently `null`.
        let timers_json = r#"[
            {"next":null,"last":1795652002000000,"unit":"ci-egress-direct-watchdog.timer","activates":"ci-egress-direct-watchdog.service"},
            {"next":null,"last":1795652002000000,"unit":"reverse-order.timer","activates":"reverse-order.service"},
            {"next":1795682665000000,"last":1795611061000000,"unit":"multi-cal.timer","activates":"multi-cal.service"}
        ]"#;
        let show_out = "\
Id=ci-egress-direct-watchdog.timer
Description=CI egress direct watchdog timer
ActiveState=active
TimersMonotonic={ OnUnitActiveUSec=1min ; next_elapse=3d 8h 36min 54.960339s }
TimersMonotonic={ OnBootUSec=3min ; next_elapse=3min }

Id=ci-egress-direct-watchdog.service
ActiveState=inactive
Result=success
ExecMainStatus=0

Id=reverse-order.timer
Description=Reverse order monotonic timer
ActiveState=active
TimersMonotonic={ OnBootUSec=3min ; next_elapse=3min }
TimersMonotonic={ OnUnitActiveUSec=5min ; next_elapse=4min 12s }

Id=reverse-order.service
ActiveState=inactive
Result=success
ExecMainStatus=0

Id=multi-cal.timer
Description=Multi calendar timer
ActiveState=active
TimersCalendar={ OnCalendar=Mon 09:00 ; next_elapse=Mon 2026-11-30 09:00:00 UTC }
TimersCalendar={ OnCalendar=Fri 09:00 ; next_elapse=Fri 2026-12-04 09:00:00 UTC }

Id=multi-cal.service
ActiveState=inactive
Result=success
ExecMainStatus=0
";
        let cfg = SchedulesConfig::default();
        let jobs = parse_systemd_timers_with_show(timers_json, show_out, &cfg);
        assert_eq!(jobs.len(), 3);

        let wd = jobs
            .iter()
            .find(|j| j.id == "ci-egress-direct-watchdog.timer")
            .unwrap();
        assert_eq!(wd.schedule_human, "Every 1m");
        assert_eq!(wd.expected_interval_secs, Some(60));
        assert!(wd.schedule_raw.contains("OnUnitActiveUSec=1min"));
        assert!(wd.schedule_raw.contains("OnBootUSec=3min"));

        let rev = jobs.iter().find(|j| j.id == "reverse-order.timer").unwrap();
        assert_eq!(rev.schedule_human, "Every 5m");
        assert_eq!(rev.expected_interval_secs, Some(300));

        let cal = jobs.iter().find(|j| j.id == "multi-cal.timer").unwrap();
        assert_eq!(cal.schedule_human, "Mon 09:00, Fri 09:00");
        assert!(cal.schedule_raw.contains("OnCalendar=Mon 09:00"));
        assert!(cal.schedule_raw.contains("OnCalendar=Fri 09:00"));
    }

    #[test]
    fn df_and_meminfo_parsers_handle_standard_outputs() {
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1 239075328 161480704 77594624 68% /\n";
        let (used, total, free) = parse_df_kp_gib(df).unwrap();
        assert!((total - 228.0).abs() < 0.2);
        assert!((used - 154.0).abs() < 0.2);
        assert!((free - 74.0).abs() < 0.2);

        let meminfo = "MemTotal:       16777216 kB\nMemAvailable:    8388608 kB\n";
        let (mused, mtotal) = parse_linux_meminfo_gib(meminfo).unwrap();
        assert_eq!((mused, mtotal), (8.0, 16.0));
    }

    #[test]
    fn review_regressions_auth_header_cpu_quota_tailscale_and_monthly_calendar() {
        // Opaque Bearer & Basic tokens after `Authorization:` must be redacted.
        let r1 = redact_secrets("Authorization: Bearer opaque-device-token-123");
        assert!(!r1.contains("opaque-device-token-123"));
        assert!(r1.contains("Bearer <redacted>"));
        let r2 = redact_secrets("Authorization: Basic dXNlcjpwYXNz");
        assert!(!r2.contains("dXNlcjpwYXNz"));
        assert!(r2.contains("Basic <redacted>"));

        // Sub-100% and compound systemd CPUQuotaPerSecUSec values.
        assert_eq!(
            parse_systemd_cpu_quota("CPUQuotaPerSecUSec=700ms"),
            Some(70.0)
        );
        assert_eq!(
            parse_systemd_cpu_quota("CPUQuotaPerSecUSec=1s 500ms"),
            Some(150.0)
        );
        assert_eq!(
            parse_systemd_cpu_quota("CPUQuotaPerSecUSec=2s"),
            Some(200.0)
        );
        assert_eq!(parse_systemd_cpu_quota("CPUQuotaPerSecUSec=infinity"), None);

        // Tailscale direct vs relay from active peers, not Self.Relay.
        let ts_direct = r#"{"Self":{"Online":true,"Relay":"nrt"},"Peer":{"a":{"Active":true,"CurAddr":"1.2.3.4:41641","Relay":"nrt"}}}"#;
        assert_eq!(
            parse_tailscale_status_json(ts_direct)
                .unwrap()
                .tailscale_mode
                .as_deref(),
            Some("direct")
        );
        let ts_relay = r#"{"Self":{"Online":true,"Relay":"nrt"},"Peer":{"a":{"Active":true,"CurAddr":"","Relay":"nrt"}}}"#;
        assert_eq!(
            parse_tailscale_status_json(ts_relay)
                .unwrap()
                .tailscale_mode
                .as_deref(),
            Some("relay")
        );

        // Monthly StartCalendarInterval does not get a 1-day expected_interval_secs.
        let monthly = serde_json::json!({"Day": 1, "Hour": 3, "Minute": 0});
        let (human, _raw, expected) = describe_calendar_dict(&monthly);
        assert_eq!(human, "Monthly day 1 03:00");
        assert_eq!(expected, None);
    }

    #[test]
    fn launchd_ignores_empty_log_mtime_and_stale_interval_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let empty_log = dir.path().join("empty.log");
        std::fs::write(&empty_log, b"").unwrap();
        let non_empty_log = dir.path().join("output.log");
        std::fs::write(&non_empty_log, b"ran ok\n").unwrap();

        let log_mtime = DateTime::<Utc>::from(
            std::fs::metadata(&non_empty_log)
                .unwrap()
                .modified()
                .unwrap(),
        );
        let cfg = SchedulesConfig::default();

        // 1. 0-byte log file (untouched since launchd created it) must not produce last_run_at
        //    or a fabricated past next_run_at.
        let empty_plist = serde_json::json!({
            "Label": "com.example.silent-empty",
            "StartInterval": 3600,
            "StandardOutPath": empty_log.to_string_lossy()
        });
        let lc = LaunchctlPrintInfo {
            state: Some("not running".to_owned()),
            runs: Some(70),
            last_exit_code: Some(0),
            ..LaunchctlPrintInfo::default()
        };
        let now_after = log_mtime + TimeDelta::hours(5);
        let empty_job = build_launchd_job(&empty_plist, Some(&lc), &cfg, now_after).unwrap();
        assert_eq!(empty_job.state, ScheduleState::Ok);
        assert_eq!(empty_job.last_run_at, None);
        assert_eq!(empty_job.next_run_at, None);

        // 2. Non-empty log file with recent mtime (< StartInterval ago) produces future next_run_at.
        let non_empty_plist = serde_json::json!({
            "Label": "com.example.silent-stale",
            "StartInterval": 3600,
            "StandardOutPath": non_empty_log.to_string_lossy()
        });
        let now_recent = log_mtime + TimeDelta::minutes(10);
        let recent_job = build_launchd_job(&non_empty_plist, Some(&lc), &cfg, now_recent).unwrap();
        assert_eq!(recent_job.last_run_at, Some(log_mtime));
        assert_eq!(
            recent_job.next_run_at,
            Some(log_mtime + TimeDelta::hours(1))
        );

        // 3. Same non-empty log file once mtime + StartInterval is in the past: keep last_run_at
        //    as lower-bound output timestamp, but omit next_run_at instead of fabricating a past
        //    due time.
        let stale_job = build_launchd_job(&non_empty_plist, Some(&lc), &cfg, now_after).unwrap();
        assert_eq!(stale_job.last_run_at, Some(log_mtime));
        assert_eq!(stale_job.next_run_at, None);

        // 4. Combined StartInterval + StartCalendarInterval picks the earlier future trigger
        //    when the interval run is upcoming, and falls back to calendar when mtime is stale.
        let combo_plist = serde_json::json!({
            "Label": "com.example.combo",
            "StartInterval": 3600,
            "StartCalendarInterval": { "Weekday": 1, "Hour": 9, "Minute": 0 },
            "StandardOutPath": non_empty_log.to_string_lossy()
        });
        let combo_recent = build_launchd_job(&combo_plist, Some(&lc), &cfg, now_recent).unwrap();
        let cal_only = compute_launchd_next_run(
            None,
            combo_plist.get("StartCalendarInterval"),
            Some(log_mtime),
            now_recent,
        )
        .unwrap();
        let expected_combo = std::cmp::min(cal_only, log_mtime + TimeDelta::hours(1));
        assert_eq!(combo_recent.next_run_at, Some(expected_combo));

        let combo_stale = build_launchd_job(&combo_plist, Some(&lc), &cfg, now_after).unwrap();
        let cal_after = compute_launchd_next_run(
            None,
            combo_plist.get("StartCalendarInterval"),
            Some(log_mtime),
            now_after,
        );
        assert_eq!(combo_stale.next_run_at, cal_after);
    }

    #[test]
    fn parse_launchctl_print_ignores_nested_state_active() {
        let raw = "\
gui/501/dev.agent-board.watch = {\n\
\tactive count = 1\n\
\tstate = running\n\
\truns = 1\n\
\tpid = 651\n\
\tlast exit code = (never exited)\n\
\tevent channels = {\n\
\t\t\"com.apple.notifyd.matching\" = {\n\
\t\t\tport = 0x1234\n\
\t\t\tstate = active\n\
\t\t}\n\
\t}\n\
\tendpoints = {\n\
\t\tstate = active\n\
\t}\n\
}\n";
        let info = parse_launchctl_print(raw);
        assert_eq!(info.state.as_deref(), Some("running"));
        assert_eq!(info.runs, Some(1));
        assert_eq!(info.pid, Some(651));
        assert_eq!(info.last_exit_code, None);
    }

    #[test]
    fn launchd_reboot_runs_zero_with_non_empty_log_is_ok_not_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let non_empty_log = dir.path().join("friction.log");
        std::fs::write(&non_empty_log, b"Judged 0 turns\n").unwrap();
        let log_mtime = DateTime::<Utc>::from(
            std::fs::metadata(&non_empty_log)
                .unwrap()
                .modified()
                .unwrap(),
        );
        let empty_log = dir.path().join("fresh.log");
        std::fs::write(&empty_log, b"").unwrap();
        let cfg = SchedulesConfig::default();
        let lc_zero = LaunchctlPrintInfo {
            state: Some("not running".to_owned()),
            runs: Some(0),
            last_exit_code: None,
            ..LaunchctlPrintInfo::default()
        };
        let now = log_mtime + TimeDelta::minutes(15);

        // Non-empty log proves the job ran prior to reboot -> Ok with log_mtime, not NeverRun.
        let plist_ran = serde_json::json!({
            "Label": "com.example.friction",
            "StartInterval": 3600,
            "StandardOutPath": non_empty_log.to_string_lossy()
        });
        let job = build_launchd_job(&plist_ran, Some(&lc_zero), &cfg, now).unwrap();
        assert_eq!(job.state, ScheduleState::Ok);
        assert_eq!(job.last_run_at, Some(log_mtime));

        // 0-byte log with runs == 0 truly never ran -> NeverRun.
        let plist_fresh = serde_json::json!({
            "Label": "com.example.fresh",
            "StartInterval": 3600,
            "StandardOutPath": empty_log.to_string_lossy()
        });
        let fresh = build_launchd_job(&plist_fresh, Some(&lc_zero), &cfg, now).unwrap();
        assert_eq!(fresh.state, ScheduleState::NeverRun);
        assert_eq!(fresh.last_run_at, None);
    }

    #[test]
    fn describe_t3_schedule_supports_weekdays_and_days_of_week() {
        let (human, interval) =
            describe_t3_schedule(r#"{"type":"fixed_time","timeOfDay":"10:00","weekdays":[1]}"#);
        assert_eq!(human, "Mon 10:00");
        assert_eq!(interval, Some(604_800));

        let (human_legacy, interval_legacy) = describe_t3_schedule(
            r#"{"type":"fixed_time","timeOfDay":"09:30","daysOfWeek":[1,5],"timezone":"Asia/Tokyo"}"#,
        );
        assert_eq!(human_legacy, "Mon,Fri 09:30 JST");
        assert_eq!(interval_legacy, Some(604_800));
    }
}
