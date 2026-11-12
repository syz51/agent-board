//! Reads the account's usage from the endpoint Claude Code's `/usage` shows, with Claude
//! Code's own sign-in, for the daemon to report (`docs/DESIGN.md`, "Usage").
//!
//! The token is only read: never refreshed, which could sign Claude Code out, and never
//! logged or sent anywhere but the usage endpoint. An expired token skips the reading until
//! Claude Code refreshes it.
//!
//! T3 Code reads the same endpoint for the same account and keeps each Claude instance's
//! newest reading in a file. The daemon takes the newest of those readings first
//! ([`read_snapshot`]), so the two do not spend the endpoint's rate limit twice.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use board_proto::{ReportUsage, Timestamp, Usage, UsageWindow};
use chrono::TimeDelta;
use serde::Deserialize;
use serde_json::Value;

/// Claude Code's usage endpoint.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The beta the endpoint asks for.
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The keychain item Claude Code keeps its sign-in in, on macOS.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Reading the sign-in and the endpoint each get this long.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The folder T3 Code keeps each provider instance's state in, as `<instance>.json`, under
/// the home directory.
const T3_CACHES: &str = ".t3/caches";
/// T3 Code's default Claude instance, which the user's own chats run on.
const T3_CLAUDE_INSTANCE: &str = "claudeAgent";
/// The driver of T3 Code's Claude instances.
const T3_CLAUDE_DRIVER: &str = "claudeAgent";
/// A snapshot dated further ahead of this device's clock than this is not believed.
const SNAPSHOT_SKEW: TimeDelta = TimeDelta::minutes(1);
/// A window whose reset time moved later by more than this is a new window.
const RESET_MOVED: TimeDelta = TimeDelta::minutes(1);

/// The usage endpoint refused a reading for its rate limit (HTTP 429): the daemon reads
/// it less often for a while.
#[derive(Debug)]
pub struct Refused {
    /// The wait the endpoint asked for (`Retry-After`, in seconds), when it gave one.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the usage endpoint answered 429 Too Many Requests")
    }
}

impl std::error::Error for Refused {}

/// Where the daemon gets the account's usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageSource {
    /// Reads nothing itself; uses the readings other devices report to the server.
    Server,
    /// Reads `url` with the token from `credentials`, and reports the reading. While one of
    /// T3 Code's `snapshots` files holds a reading that is new enough, takes the newest such
    /// reading instead.
    ClaudeCode {
        url: String,
        credentials: Credentials,
        snapshots: Vec<PathBuf>,
    },
}

/// Where Claude Code keeps its sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// The macOS login keychain, read with `security`.
    Keychain,
    /// A JSON file, as Claude Code writes on Linux (`~/.claude/.credentials.json`).
    File(PathBuf),
}

impl UsageSource {
    /// Claude Code's endpoint and this platform's sign-in store; [`UsageSource::Server`]
    /// without a home directory to find it in. T3 Code's readings come from its default
    /// Claude instance and from `thread_instance`, the instance the daemon starts its
    /// threads on (`[thread_model]`): readings taken during those threads' turns go to that
    /// instance's file.
    pub fn claude_code(thread_instance: Option<&str>) -> Self {
        let home = dirs::home_dir();
        let credentials = if cfg!(target_os = "macos") {
            Credentials::Keychain
        } else {
            match &home {
                Some(home) => Credentials::File(home.join(".claude/.credentials.json")),
                None => return Self::Server,
            }
        };
        Self::ClaudeCode {
            url: USAGE_URL.to_owned(),
            credentials,
            snapshots: home
                .map(|home| {
                    let caches = home.join(T3_CACHES);
                    let mut instances = vec![T3_CLAUDE_INSTANCE];
                    instances.extend(thread_instance.filter(|i| *i != T3_CLAUDE_INSTANCE));
                    instances
                        .into_iter()
                        .map(|instance| caches.join(format!("{instance}.json")))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// The parts of the sign-in this needs.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    claude_ai_oauth: Option<OAuth>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuth {
    access_token: String,
    /// Milliseconds since the epoch.
    expires_at: Option<i64>,
}

/// The endpoint's answer: each window's percent used and when it resets. Other windows
/// (per model) are not read.
#[derive(Deserialize)]
struct Answer {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    utilization: Option<f64>,
    resets_at: Option<Timestamp>,
}

impl Window {
    fn into_proto(self) -> Option<UsageWindow> {
        Some(UsageWindow {
            percent: self.utilization?,
            resets_at: self.resets_at,
        })
    }
}

/// Reads the usage at `now`: `None` when Claude Code is not signed in here or its token
/// has expired.
pub async fn read(
    url: &str,
    credentials: &Credentials,
    now: Timestamp,
) -> Result<Option<ReportUsage>> {
    let Some(token) = token(credentials, now).await? else {
        return Ok(None);
    };
    let http = reqwest::Client::builder()
        .timeout(READ_TIMEOUT)
        .build()
        .context("building the usage client")?;
    let resp = http
        .get(url)
        .bearer_auth(token)
        .header("anthropic-beta", OAUTH_BETA)
        .send()
        .await
        .context("reading the usage endpoint")?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()?.trim().parse().ok())
            .map(Duration::from_secs);
        return Err(Refused { retry_after }.into());
    }
    if !status.is_success() {
        bail!("the usage endpoint answered {status}");
    }
    let answer: Answer = resp.json().await.context("reading the usage answer")?;
    let report = ReportUsage {
        five_hour: answer.five_hour.and_then(Window::into_proto),
        seven_day: answer.seven_day.and_then(Window::into_proto),
        read_at: now,
    };
    // An empty reading would release every board the last one holds.
    if report.five_hour.is_none() && report.seven_day.is_none() {
        bail!(
            "the usage answer has no five_hour or seven_day utilization: its format may have changed"
        );
    }
    Ok(Some(report))
}

/// The access token, unless there is none or it has expired by `now`.
async fn token(credentials: &Credentials, now: Timestamp) -> Result<Option<String>> {
    let text = match credentials {
        Credentials::Keychain => {
            let read = tokio::process::Command::new("/usr/bin/security")
                .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(READ_TIMEOUT, read)
                .await
                .context("reading the keychain: no answer")?
                .context("running security")?;
            // Exit 44: no such item, Claude Code is not signed in here.
            if out.status.code() == Some(44) {
                return Ok(None);
            }
            if !out.status.success() {
                bail!(
                    "reading the keychain item {KEYCHAIN_SERVICE:?}: security exited {}",
                    out.status
                );
            }
            String::from_utf8(out.stdout).context("the keychain item is not UTF-8")?
        }
        Credentials::File(path) => match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        },
    };
    // The error says where, never what: the text holds the token.
    let stored: Stored = serde_json::from_str(text.trim())
        .map_err(|_| anyhow::anyhow!("Claude Code's sign-in is not the JSON expected"))?;
    let Some(oauth) = stored.claude_ai_oauth else {
        return Ok(None);
    };
    let expired = oauth
        .expires_at
        .is_some_and(|ms| ms <= now.timestamp_millis());
    Ok((!expired).then_some(oauth.access_token))
}

/// The readings in T3 Code's snapshot files at `paths`, each dated when T3 Code took it.
/// The first path is the default Claude instance's. A later file counts only while it is
/// signed in to the same workspace as the first, so another account's usage is never
/// reported. See [`read_snapshot`] for when a file gives no reading.
pub async fn read_snapshots(paths: &[PathBuf], now: Timestamp) -> Vec<ReportUsage> {
    let mut files = Vec::new();
    for path in paths {
        files.push(read_snapshot(path, now).await);
    }
    let workspace = files.first().cloned().flatten().and_then(|(_, w)| w);
    files
        .into_iter()
        .enumerate()
        .filter_map(|(i, file)| {
            let (reading, ws) = file?;
            (i == 0 || (ws.is_some() && ws == workspace)).then_some(reading)
        })
        .collect()
}

/// The reading in T3 Code's snapshot file at `path`, dated when T3 Code took it, with the
/// workspace its instance is signed in to. `None` when there is no file, it is not a
/// Claude instance's, T3 Code marks the reading unavailable, or the usage part is not in
/// the shape expected: the file is T3 Code's own format and may change, so the caller
/// then reads the endpoint. Nothing but the usage part, the driver and the workspace id is
/// read, and nothing of the file is logged.
async fn read_snapshot(path: &Path, now: Timestamp) -> Option<(ReportUsage, Option<String>)> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    snapshot_file(&text, now)
}

fn snapshot_file(text: &str, now: Timestamp) -> Option<(ReportUsage, Option<String>)> {
    let file: Value = serde_json::from_str(text).ok()?;
    if file
        .get("driver")
        .and_then(Value::as_str)
        .is_some_and(|driver| driver != T3_CLAUDE_DRIVER)
    {
        return None;
    }
    let workspace = file
        .pointer("/auth/workspaceId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let limits = file.get("usageLimits")?;
    // A failed probe keeps older windows or none.
    if limits.get("unavailable").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let read_at: Timestamp = limits.get("checkedAt")?.as_str()?.parse().ok()?;
    if read_at > now + SNAPSHOT_SKEW {
        return None;
    }
    let windows = limits.get("windows")?.as_array()?;
    let window = |id: &str| {
        let found = windows
            .iter()
            .find(|w| w.get("id").and_then(Value::as_str) == Some(id))?;
        let percent = found.get("usedPercent")?.as_f64()?;
        Some(UsageWindow {
            percent,
            resets_at: found
                .get("resetsAt")
                .and_then(Value::as_str)
                .and_then(|at| at.parse().ok()),
        })
    };
    let report = ReportUsage {
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
        read_at,
    };
    (report.five_hour.is_some() || report.seven_day.is_some()).then_some((report, workspace))
}

/// Whether `snapshot` can follow `known`, the newest reading the daemon has. T3 Code can
/// date an old reading anew, and within one window the usage only rises. So a snapshot
/// follows `known` when it has each window `known` has, and each is at least as high as
/// in `known` or belongs to a later window: its reset time is later, or it has none and
/// the known window has ended.
pub fn follows(snapshot: &ReportUsage, known: &Usage) -> bool {
    let window = |new: &Option<UsageWindow>, old: &Option<UsageWindow>| match (new, old) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(new), Some(old)) => {
            // T3 Code may round the percent.
            new.percent >= old.percent.floor()
                || old.resets_at.is_some_and(|ended| match new.resets_at {
                    Some(at) => at > ended + RESET_MOVED,
                    None => ended <= snapshot.read_at,
                })
        }
    };
    window(&snapshot.five_hour, &known.five_hour) && window(&snapshot.seven_day, &known.seven_day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file T3 Code wrote, with every value outside the usage part replaced.
    const RECORDED: &str = include_str!("../tests/data/t3-claude-snapshot.json");

    fn snapshot_reading(text: &str, now: Timestamp) -> Option<ReportUsage> {
        snapshot_file(text, now).map(|(reading, _)| reading)
    }

    #[test]
    fn the_thread_instance_s_file_is_read_next_to_the_default_one() {
        let names = |source: UsageSource| match source {
            UsageSource::ClaudeCode { snapshots, .. } => snapshots
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            UsageSource::Server => Vec::new(),
        };
        assert_eq!(names(UsageSource::claude_code(None)), ["claudeAgent.json"]);
        assert_eq!(
            names(UsageSource::claude_code(Some("claude-agents"))),
            ["claudeAgent.json", "claude-agents.json"]
        );
        assert_eq!(
            names(UsageSource::claude_code(Some("claudeAgent"))),
            ["claudeAgent.json"]
        );
    }

    #[tokio::test]
    async fn another_instance_counts_only_when_it_is_a_claude_one_on_the_same_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let now = at("2026-11-11T11:24:00Z");
        let file = |name: &str, driver: &str, workspace: &str, checked: &str| {
            let path = dir.path().join(name);
            let text = serde_json::json!({
                "driver": driver, "auth": { "workspaceId": workspace },
                "usageLimits": { "checkedAt": checked,
                    "windows": [{ "id": "five_hour", "usedPercent": 38 }] },
            });
            std::fs::write(&path, text.to_string()).unwrap();
            path
        };
        let default = file(
            "claudeAgent.json",
            "claudeAgent",
            "ws-1",
            "2026-11-11T11:20:00Z",
        );
        let times = |paths: Vec<PathBuf>| async move {
            read_snapshots(&paths, now)
                .await
                .into_iter()
                .map(|r| r.read_at)
                .collect::<Vec<_>>()
        };

        let same = file("a.json", "claudeAgent", "ws-1", "2026-11-11T11:23:00Z");
        assert_eq!(
            times(vec![default.clone(), same]).await,
            [at("2026-11-11T11:20:00Z"), at("2026-11-11T11:23:00Z")]
        );
        for other in [
            file("b.json", "claudeAgent", "ws-2", "2026-11-11T11:23:00Z"),
            file("c.json", "codex", "ws-1", "2026-11-11T11:23:00Z"),
        ] {
            assert_eq!(
                times(vec![default.clone(), other]).await,
                [at("2026-11-11T11:20:00Z")]
            );
        }
        // Without the default instance's reading, no workspace is known to match.
        let same = file("d.json", "claudeAgent", "ws-1", "2026-11-11T11:23:00Z");
        assert!(
            times(vec![dir.path().join("none.json"), same])
                .await
                .is_empty()
        );
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn window(percent: f64, resets_at: &str) -> Option<UsageWindow> {
        Some(UsageWindow {
            percent,
            resets_at: Some(at(resets_at)),
        })
    }

    #[test]
    fn a_recorded_snapshot_gives_both_windows_and_its_own_time() {
        let now = at("2026-11-11T11:24:00Z");
        assert_eq!(
            snapshot_reading(RECORDED, now),
            Some(ReportUsage {
                five_hour: window(38.0, "2026-11-11T14:20:00.312Z"),
                seven_day: window(10.0, "2026-11-15T01:00:00.312Z"),
                read_at: at("2026-11-11T11:22:59.448Z"),
            })
        );
    }

    #[test]
    fn a_snapshot_in_another_shape_is_no_reading() {
        let now = at("2026-11-11T11:24:00Z");
        let reading = |limits: Value| {
            snapshot_reading(
                &serde_json::json!({ "usageLimits": limits }).to_string(),
                now,
            )
        };
        let checked = "2026-11-11T11:22:59.448Z";
        let five = serde_json::json!({ "id": "five_hour", "usedPercent": 38 });

        // One window, no reset time, unknown fields and windows: still a reading.
        let lenient = reading(serde_json::json!({
            "checkedAt": checked, "added": true,
            "windows": [{ "id": "monthly_spend", "usedPercent": 3 }, "odd", five],
        }))
        .unwrap();
        assert_eq!(lenient.five_hour.unwrap().percent, 38.0);
        assert_eq!(lenient.seven_day, None);

        for (why, limits) in [
            (
                "a failed probe",
                serde_json::json!({
                    "checkedAt": checked, "windows": [five], "unavailable": { "reason": "probeFailed" },
                }),
            ),
            ("no time", serde_json::json!({ "windows": [five] })),
            (
                "a time that is none",
                serde_json::json!({ "checkedAt": 5, "windows": [five] }),
            ),
            (
                "a time ahead of the clock",
                serde_json::json!({
                    "checkedAt": "2026-11-11T11:30:00Z", "windows": [five],
                }),
            ),
            (
                "no window read here",
                serde_json::json!({
                    "checkedAt": checked, "windows": [{ "id": "seven_day_other", "usedPercent": 3 }],
                }),
            ),
            (
                "a percent that is no number",
                serde_json::json!({
                    "checkedAt": checked, "windows": [{ "id": "five_hour", "usedPercent": "38" }],
                }),
            ),
            (
                "windows that are no list",
                serde_json::json!({ "checkedAt": checked, "windows": {} }),
            ),
        ] {
            assert_eq!(reading(limits), None, "{why}");
        }
        assert_eq!(snapshot_reading("{}", now), None);
        assert_eq!(snapshot_reading("not JSON", now), None);
        assert_eq!(snapshot_reading("", now), None);
    }

    #[test]
    fn a_snapshot_follows_a_reading_unless_a_window_went_down_without_a_reset() {
        let known = Usage {
            five_hour: window(48.4, "2026-11-11T14:20:00.030Z"),
            seven_day: window(13.0, "2026-11-15T01:00:00.030Z"),
            read_at: at("2026-11-11T11:20:52Z"),
            device: "air".parse().unwrap(),
            pace: None,
        };
        let snapshot = |five_hour, seven_day, read_at: &str| ReportUsage {
            five_hour,
            seven_day,
            read_at: at(read_at),
        };
        let week = || window(13.0, "2026-11-15T01:00:00.312Z");
        let later = "2026-11-11T11:22:59Z";

        // Higher, or the same after rounding.
        let same = snapshot(window(48.0, "2026-11-11T14:20:00.312Z"), week(), later);
        assert!(follows(&same, &known));
        // Lower in the same window: an old reading under a new date.
        let old = snapshot(window(38.0, "2026-11-11T14:20:00.312Z"), week(), later);
        assert!(!follows(&old, &known));
        let old_week = snapshot(
            window(50.0, "2026-11-11T14:20:00.312Z"),
            window(10.0, "2026-11-15T01:00:00.312Z"),
            later,
        );
        assert!(!follows(&old_week, &known));
        // Lower with a later reset time: the window was reset early.
        let reset = snapshot(window(2.0, "2026-11-11T16:25:00Z"), week(), later);
        assert!(follows(&reset, &known));
        // Lower after the known window ended, also without a reset time.
        let ended = snapshot(
            Some(UsageWindow {
                percent: 0.0,
                resets_at: None,
            }),
            week(),
            "2026-11-11T14:21:00Z",
        );
        assert!(follows(&ended, &known));
        // The old window's reading dated after its end is still the old reading.
        let late = snapshot(
            window(38.0, "2026-11-11T14:20:00.312Z"),
            week(),
            "2026-11-11T14:21:00Z",
        );
        assert!(!follows(&late, &known));
        // A window missing: reporting it would drop that window's hold.
        assert!(!follows(&snapshot(None, week(), later), &known));
        // Nothing known for a window: any snapshot follows.
        let unknown = Usage {
            five_hour: None,
            ..known.clone()
        };
        assert!(follows(&old, &unknown));
    }
}
