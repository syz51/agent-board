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
use std::process::Stdio;
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
/// Timeout for the official `antigravity -p /usage --output-format json` subprocess.
pub const ANTIGRAVITY_CLI_TIMEOUT: Duration = Duration::from_secs(20);
/// `--print-timeout` passed to `antigravity -p /usage`.
const ANTIGRAVITY_PRINT_TIMEOUT: &str = "15s";

/// The folder T3 Code keeps each provider instance's state in, as `<instance>.json`, under
/// the home directory.
const T3_CACHES: &str = ".t3/caches";
/// T3 Code's default Claude instance, which the user's own chats run on.
const T3_CLAUDE_INSTANCE: &str = "claudeAgent";
/// The driver of T3 Code's Claude instances.
const T3_CLAUDE_DRIVER: &str = "claudeAgent";
/// T3 Code's default Antigravity instance.
const T3_ANTIGRAVITY_INSTANCE: &str = "antigravity";
/// The driver of T3 Code's Antigravity instances.
const T3_ANTIGRAVITY_DRIVER: &str = "antigravity";
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
    /// Reads Antigravity usage via the official `agy`/`antigravity -p /usage --output-format json`
    /// CLI, falling back to any legacy T3 Code cache files (`~/.t3/caches/<instance>.json`) while
    /// they still carry `usageLimits`.
    Antigravity {
        cli_bin: Option<PathBuf>,
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

    /// Official Antigravity CLI usage reader (`-p /usage --output-format json`) with optional
    /// legacy T3 Code cache files (`~/.t3/caches/<instance>.json`).
    pub fn antigravity(thread_instance: Option<&str>, configured_bin: Option<PathBuf>) -> Self {
        let cli_bin = resolve_antigravity_cli_bin(configured_bin);
        let snapshots: Vec<PathBuf> = dirs::home_dir()
            .map(|home| {
                let caches = home.join(T3_CACHES);
                let mut instances = vec![T3_ANTIGRAVITY_INSTANCE];
                instances.extend(thread_instance.filter(|i| *i != T3_ANTIGRAVITY_INSTANCE));
                instances
                    .into_iter()
                    .map(|instance| caches.join(format!("{instance}.json")))
                    .collect()
            })
            .unwrap_or_default();
        if cli_bin.is_none() && snapshots.is_empty() {
            return Self::Server;
        }
        Self::Antigravity { cli_bin, snapshots }
    }

    /// Chooses the usage source for `thread_instance` (from `[thread_model]`).
    pub fn for_provider(thread_instance: Option<&str>, configured_bin: Option<PathBuf>) -> Self {
        if thread_instance.is_some_and(|p| p.to_ascii_lowercase().starts_with("antigravity")) {
            Self::antigravity(thread_instance, configured_bin)
        } else {
            Self::claude_code(thread_instance)
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

#[derive(Deserialize)]
struct AgyCliOutput {
    status: Option<String>,
    error: Option<String>,
    command: Option<AgyCliCommand>,
}

#[derive(Deserialize)]
struct AgyCliCommand {
    data: Option<AgyCliUsageData>,
}

#[derive(Deserialize)]
struct AgyCliUsageData {
    #[serde(default)]
    groups: Vec<AgyCliUsageGroup>,
    #[serde(default)]
    buckets: Vec<AgyCliUsageBucket>,
}

#[derive(Deserialize)]
struct AgyCliUsageGroup {
    #[serde(default)]
    buckets: Vec<AgyCliUsageBucket>,
}

#[derive(Deserialize)]
struct AgyCliUsageBucket {
    id: Option<String>,
    remaining_fraction: Option<f64>,
    reset_time: Option<String>,
}

fn is_interactive_oauth_launch(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("consumeroauth: starting oauth")
        || lower.contains("starting oauth flow")
        || lower.contains("open this url in your browser")
        || lower.contains("opening browser for authentication")
}

fn is_antigravity_auth_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("authentication required")
        || lower.contains("auth_required")
        || lower.contains("authentication failed")
        || lower.contains("please sign in")
        || lower.contains("not authenticated")
        || is_interactive_oauth_launch(&lower)
}

fn looks_like_antigravity_bin(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower == "agy" || lower == "agy.exe" || lower.contains("antigravity")
        })
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exe_name = (cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe"))
        .then(|| format!("{name}.exe"));
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if let Some(exe) = &exe_name {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolves the official first-party `agy` / `antigravity` CLI binary to query `-p /usage`:
/// prefers a configured `[acp_runner].bin` pointing to Antigravity, then `agy` / `antigravity`
/// on `PATH` (or `~/.local/bin/agy`).
pub fn resolve_antigravity_cli_bin(configured_bin: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(bin) = configured_bin.filter(|p| looks_like_antigravity_bin(p)) {
        if bin.is_file() {
            return Some(bin);
        }
        if bin.components().count() == 1
            && let Some(name) = bin.to_str()
            && let Some(found) = find_in_path(name)
        {
            return Some(found);
        }
    }
    for name in ["agy", "antigravity"] {
        if let Some(found) = find_in_path(name) {
            return Some(found);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let local_agy = home.join(if cfg!(windows) {
            ".local/bin/agy.exe"
        } else {
            ".local/bin/agy"
        });
        if local_agy.is_file() {
            return Some(local_agy);
        }
    }
    None
}

async fn kill_usage_child(child: &mut tokio::process::Child, pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(raw) = pgid.and_then(|g| i32::try_from(g).ok()).filter(|&p| p > 0)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    #[cfg(not(unix))]
    let _ = pgid;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Reads Antigravity quota via the official first-party CLI (`<bin> -p /usage --output-format
/// json --print-timeout 15s`). Uses the CLI's own native sign-in, never extracts credentials,
/// never launches an interactive browser OAuth flow, and never calls Google's internal APIs
/// directly. Returns `Ok(None)` when the CLI is not signed in on this device.
pub async fn read_antigravity_cli(bin: &Path, now: Timestamp) -> Result<Option<ReportUsage>> {
    read_antigravity_cli_with_timeout(bin, now, ANTIGRAVITY_CLI_TIMEOUT).await
}

/// Like [`read_antigravity_cli`] with an explicit subprocess `timeout`.
pub async fn read_antigravity_cli_with_timeout(
    bin: &Path,
    now: Timestamp,
    timeout: Duration,
) -> Result<Option<ReportUsage>> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

    let mut cmd = tokio::process::Command::new(bin);
    cmd.args([
        "-p",
        "/usage",
        "--output-format",
        "json",
        "--print-timeout",
        ANTIGRAVITY_PRINT_TIMEOUT,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    // Never trigger board hooks or interactive browser OAuth launches from a background probe.
    .env("AGENT_BOARD_HOOKS", "off")
    .env("NO_BROWSER", "1")
    .env("BROWSER", "false")
    // Clear SSH session markers so the official CLI uses native keychain sign-in when present,
    // and clear overridden profile/wrapper variables.
    .env_remove("SSH_CLIENT")
    .env_remove("SSH_CONNECTION")
    .env_remove("SSH_TTY")
    .env_remove("GEMINI_HOME")
    .env_remove("CLAUDECODE")
    .env_remove("ELECTRON_RUN_AS_NODE")
    .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {} -p /usage", bin.display()))?;
    let pgid = child.id();
    let mut stdout_pipe = child
        .stdout
        .take()
        .context("antigravity -p /usage stdout not piped")?;
    let stderr_pipe = child
        .stderr
        .take()
        .context("antigravity -p /usage stderr not piped")?;

    let mut stdout_fut = Box::pin(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let mut stderr_reader = tokio::io::BufReader::new(stderr_pipe);
    let mut stderr_buf = Vec::new();
    let mut line_buf = Vec::new();
    let mut stdout_buf: Option<Vec<u8>> = None;
    let mut stderr_done = false;
    let mut oauth_aborted = false;

    let waited = tokio::time::timeout(timeout, async {
        while stdout_buf.is_none() || !stderr_done {
            tokio::select! {
                buf = &mut stdout_fut, if stdout_buf.is_none() => {
                    stdout_buf = Some(buf);
                }
                res = stderr_reader.read_until(b'\n', &mut line_buf), if !stderr_done => {
                    match res {
                        Ok(0) | Err(_) => stderr_done = true,
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&line_buf);
                            if is_interactive_oauth_launch(&line) {
                                oauth_aborted = true;
                                break;
                            }
                            stderr_buf.extend_from_slice(&line_buf);
                            line_buf.clear();
                        }
                    }
                }
            }
        }
        if oauth_aborted {
            return Ok::<Option<std::process::ExitStatus>, std::io::Error>(None);
        }
        child.wait().await.map(Some)
    })
    .await;

    let status = match waited {
        Ok(Ok(Some(status))) => status,
        Ok(Ok(None)) => {
            kill_usage_child(&mut child, pgid).await;
            return Ok(None);
        }
        Ok(Err(err)) => {
            kill_usage_child(&mut child, pgid).await;
            return Err(err).with_context(|| format!("running {} -p /usage", bin.display()));
        }
        Err(_) => {
            kill_usage_child(&mut child, pgid).await;
            bail!("running antigravity -p /usage: timed out");
        }
    };

    let stdout_bytes = stdout_buf.unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_buf);
    if status.success() {
        return parse_antigravity_cli_usage(&stdout, now);
    }
    if !stdout.trim().is_empty()
        && let Ok(parsed) = parse_antigravity_cli_usage(&stdout, now)
    {
        return Ok(parsed);
    }
    if is_antigravity_auth_error(&stdout) || is_antigravity_auth_error(&stderr) {
        return Ok(None);
    }
    let detail = stderr.trim();
    let snippet: String = detail.chars().take(200).collect();
    if snippet.is_empty() {
        bail!("{} -p /usage exited {}", bin.display(), status);
    }
    bail!("{} -p /usage exited {}: {snippet}", bin.display(), status);
}

fn parse_agy_cli_output(trimmed: &str) -> std::result::Result<AgyCliOutput, serde_json::Error> {
    let first_err = match serde_json::from_str::<AgyCliOutput>(trimmed) {
        Ok(parsed) => return Ok(parsed),
        Err(err) => err,
    };
    // Handle leading/trailing CLI notice lines around either single-line or pretty-printed JSON.
    let mut offset = 0usize;
    for line in trimmed.split_inclusive('\n') {
        if line.trim_start().starts_with('{') {
            let mut de = serde_json::Deserializer::from_str(&trimmed[offset..]).into_iter();
            if let Some(Ok(parsed)) = de.next() {
                return Ok(parsed);
            }
        }
        offset += line.len();
    }
    Err(first_err)
}

/// Parses the JSON output of `antigravity -p /usage --output-format json`.
pub fn parse_antigravity_cli_usage(text: &str, now: Timestamp) -> Result<Option<ReportUsage>> {
    let trimmed = text.trim();
    let parsed =
        parse_agy_cli_output(trimmed).context("parsing antigravity -p /usage JSON output")?;
    if parsed.status.as_deref() != Some("SUCCESS") {
        let err_msg = parsed.error.as_deref().unwrap_or("");
        if is_antigravity_auth_error(err_msg) {
            return Ok(None);
        }
        bail!(
            "antigravity -p /usage returned status {:?}: {}",
            parsed.status.as_deref().unwrap_or("unknown"),
            err_msg
        );
    }
    let Some(data) = parsed.command.and_then(|c| c.data) else {
        bail!("antigravity -p /usage output has no command.data: its format may have changed");
    };
    let mut five_hour = None;
    let mut seven_day = None;
    let buckets = data
        .groups
        .into_iter()
        .flat_map(|g| g.buckets)
        .chain(data.buckets);
    for bucket in buckets {
        let Some(id) = bucket.id.as_deref() else {
            continue;
        };
        let Some(frac) = bucket.remaining_fraction.filter(|f| f.is_finite()) else {
            continue;
        };
        let window = UsageWindow {
            percent: (1.0 - frac.clamp(0.0, 1.0)) * 100.0,
            resets_at: bucket
                .reset_time
                .as_deref()
                .and_then(|s| s.parse::<Timestamp>().ok())
                .filter(|t| t.timestamp() > 0),
        };
        match id {
            "gemini-5h" => five_hour = Some(window),
            "gemini-weekly" => seven_day = Some(window),
            _ => {}
        }
    }
    if five_hour.is_none() && seven_day.is_none() {
        bail!(
            "antigravity -p /usage returned no gemini-5h or gemini-weekly buckets: its format may have changed"
        );
    }
    Ok(Some(ReportUsage {
        five_hour,
        seven_day,
        read_at: now,
    }))
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
    let driver = file.get("driver").and_then(Value::as_str);
    let is_antigravity = driver == Some(T3_ANTIGRAVITY_DRIVER);
    if driver.is_some_and(|d| d != T3_CLAUDE_DRIVER && d != T3_ANTIGRAVITY_DRIVER) {
        return None;
    }
    let workspace = file
        .pointer("/auth/workspaceId")
        .and_then(Value::as_str)
        .or_else(|| {
            file.pointer("/usageLimits/credentialFingerprint")
                .and_then(Value::as_str)
        })
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
    let report = if is_antigravity {
        ReportUsage {
            five_hour: window("gemini-5h"),
            seven_day: window("gemini-weekly"),
            read_at,
        }
    } else {
        ReportUsage {
            five_hour: window("five_hour"),
            seven_day: window("seven_day"),
            read_at,
        }
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
            UsageSource::ClaudeCode { snapshots, .. }
            | UsageSource::Antigravity { snapshots, .. } => snapshots
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
            reserve_until: None,
            reserve_prev: None,
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

    #[test]
    fn antigravity_snapshot_reads_gemini_windows_and_ignores_3p() {
        let now: Timestamp = "2026-11-23T10:30:00Z".parse().unwrap();
        let text = r#"{
            "driver": "antigravity",
            "instanceId": "antigravity",
            "usageLimits": {
                "checkedAt": "2026-11-23T10:26:44Z",
                "credentialFingerprint": "fp-abc",
                "windows": [
                    {"id": "3p-5h", "kind": "session", "usedPercent": 99.0},
                    {"id": "gemini-5h", "kind": "session", "usedPercent": 12.5, "resetsAt": "2026-11-23T14:51:17Z"},
                    {"id": "3p-weekly", "kind": "weekly", "usedPercent": 88.0},
                    {"id": "gemini-weekly", "kind": "weekly", "usedPercent": 3.25, "resetsAt": "2026-11-30T09:51:17Z"}
                ]
            }
        }"#;
        let (report, ws) = snapshot_file(text, now).expect("valid antigravity snapshot");
        assert_eq!(ws.as_deref(), Some("fp-abc"));
        assert_eq!(report.five_hour.unwrap().percent, 12.5);
        assert_eq!(report.seven_day.unwrap().percent, 3.25);
    }

    #[test]
    fn antigravity_cli_parses_real_json_and_ignores_3p_and_epoch_resets() {
        let now = at("2026-11-26T07:38:00Z");
        let text = r#"{
            "conversation_id": "",
            "status": "SUCCESS",
            "response": "Gemini Models\tWeekly Limit Remaining\t97%\t2026-12-03T07:03:36Z\n",
            "duration_seconds": 0,
            "num_turns": 0,
            "usage": {"input_tokens": 0, "output_tokens": 0, "thinking_tokens": 0, "cache_read_tokens": 0, "total_tokens": 0},
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.9739076495170593, "reset_time": "2026-12-03T07:03:36Z"},
                                {"id": "gemini-5h", "window": "5h", "remaining_fraction": 0.8956304788589478, "reset_time": "2026-11-26T12:03:36Z"}
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "buckets": [
                                {"id": "3p-weekly", "window": "weekly", "remaining_fraction": 0.0, "reset_time": "1970-01-01T00:00:00Z"},
                                {"id": "3p-5h", "window": "5h", "remaining_fraction": 0.0, "reset_time": "1970-01-01T00:00:00Z"}
                            ]
                        }
                    ]
                }
            }
        }"#;
        let report = parse_antigravity_cli_usage(text, now)
            .expect("valid JSON")
            .expect("some reading");
        assert_eq!(report.read_at, now);
        let five = report.five_hour.expect("five_hour");
        let week = report.seven_day.expect("seven_day");
        assert!((five.percent - 10.43695211410522).abs() < 1e-6);
        assert_eq!(five.resets_at, Some(at("2026-11-26T12:03:36Z")));
        assert!((week.percent - 2.60923504829407).abs() < 1e-6);
        assert_eq!(week.resets_at, Some(at("2026-12-03T07:03:36Z")));

        // Epoch sentinel reset_time ("1970-01-01T00:00:00Z") is dropped as None.
        let untouched = r#"{
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [{
                        "buckets": [
                            {"id": "gemini-weekly", "remaining_fraction": 1.0, "reset_time": "1970-01-01T00:00:00Z"}
                        ]
                    }]
                }
            }
        }"#;
        let report = parse_antigravity_cli_usage(untouched, now)
            .expect("valid JSON")
            .expect("some reading");
        assert_eq!(report.five_hour, None);
        assert_eq!(report.seven_day.unwrap().percent, 0.0);
        assert_eq!(report.seven_day.unwrap().resets_at, None);
    }

    #[test]
    fn antigravity_cli_unauthenticated_returns_none_and_missing_buckets_errors() {
        let now = at("2026-11-26T07:38:00Z");
        let unauth = r#"{"status":"ERROR","error":"authentication failed or timed out"}"#;
        assert_eq!(parse_antigravity_cli_usage(unauth, now).unwrap(), None);

        let other_err = r#"{"status":"ERROR","error":"internal error"}"#;
        assert!(parse_antigravity_cli_usage(other_err, now).is_err());

        let no_gemini = r#"{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"buckets":[{"id":"3p-weekly","remaining_fraction":0.5}]}]}}}"#;
        assert!(parse_antigravity_cli_usage(no_gemini, now).is_err());

        // Non-RFC3339 or empty reset_time on any bucket must not abort parsing, and pretty-printed
        // JSON preceded by a stdout notice line must still parse cleanly.
        let pretty_with_preamble = "Checking quota...\n{\n  \"status\": \"SUCCESS\",\n  \"command\": {\n    \"name\": \"usage\",\n    \"data\": {\n      \"groups\": [\n        {\n          \"buckets\": [\n            {\"id\": \"3p-weekly\", \"remaining_fraction\": 1.0, \"reset_time\": \"\"},\n            {\"id\": \"gemini-weekly\", \"remaining_fraction\": 0.9, \"reset_time\": \"not-a-timestamp\"},\n            {\"id\": \"gemini-5h\", \"remaining_fraction\": 0.8, \"reset_time\": \"2026-11-26T12:00:00Z\"}\n          ]\n        }\n      ]\n    }\n  }\n}\n";
        let parsed = parse_antigravity_cli_usage(pretty_with_preamble, now)
            .expect("pretty JSON with preamble should parse")
            .expect("some reading");
        assert!((parsed.seven_day.unwrap().percent - 10.0).abs() < 1e-6);
        assert_eq!(parsed.seven_day.unwrap().resets_at, None);
        assert!((parsed.five_hour.unwrap().percent - 20.0).abs() < 1e-6);
        assert_eq!(
            parsed.five_hour.unwrap().resets_at,
            Some(at("2026-11-26T12:00:00Z"))
        );
    }

    #[test]
    fn antigravity_bin_resolution_filters_non_antigravity() {
        assert!(looks_like_antigravity_bin(Path::new("/usr/local/bin/agy")));
        assert!(looks_like_antigravity_bin(Path::new("agy.exe")));
        assert!(looks_like_antigravity_bin(Path::new(
            "/opt/tools/antigravity"
        )));
        assert!(!looks_like_antigravity_bin(Path::new(
            "/usr/local/bin/codex"
        )));
        assert!(!looks_like_antigravity_bin(Path::new(
            "/usr/local/bin/claude"
        )));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_antigravity_cli_invokes_binary_with_clean_env() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("antigravity");
        std::fs::write(
            &script,
            r#"#!/bin/sh
set -eu
if [ "$1" != "-p" ] || [ "$2" != "/usage" ] || [ "$3" != "--output-format" ] || [ "$4" != "json" ]; then
  echo "unexpected args: $*" >&2
  exit 2
fi
if [ "${AGENT_BOARD_HOOKS:-}" != "off" ]; then
  echo "missing AGENT_BOARD_HOOKS=off" >&2
  exit 3
fi
if [ "${NO_BROWSER:-}" != "1" ] || [ "${BROWSER:-}" != "false" ]; then
  echo "missing non-interactive browser guards" >&2
  exit 5
fi
if [ -n "${SSH_CLIENT:-}${SSH_CONNECTION:-}${SSH_TTY:-}${GEMINI_HOME:-}" ]; then
  echo "uncleaned env" >&2
  exit 4
fi
printf '%s\n' '{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"buckets":[{"id":"gemini-5h","remaining_fraction":0.8,"reset_time":"2026-11-26T12:00:00Z"},{"id":"gemini-weekly","remaining_fraction":0.75,"reset_time":"2026-12-03T07:00:00Z"}]}]}}}'
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let now = at("2026-11-26T07:40:00Z");
        let report = read_antigravity_cli(&script, now)
            .await
            .expect("cli ok")
            .expect("reading");
        assert_eq!(report.read_at, now);
        assert!((report.five_hour.unwrap().percent - 20.0).abs() < 1e-6);
        assert!((report.seven_day.unwrap().percent - 25.0).abs() < 1e-6);

        // Incidental stderr warning mentioning "authentication failed" must not discard a
        // successful stdout JSON reading.
        let noisy = dir.path().join("antigravity-noisy");
        std::fs::write(
            &noisy,
            r#"#!/bin/sh
echo "WARN mcp plugin: authentication failed for optional plugin" >&2
printf '%s\n' '{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"buckets":[{"id":"gemini-weekly","remaining_fraction":0.5,"reset_time":"2026-12-03T07:00:00Z"}]}]}}}'
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&noisy).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&noisy, perms).unwrap();
        let noisy_report = read_antigravity_cli(&noisy, now)
            .await
            .expect("noisy stderr ok")
            .expect("reading preserved");
        assert!((noisy_report.seven_day.unwrap().percent - 50.0).abs() < 1e-6);

        // Non-zero exit with auth error returns Ok(None); non-zero exit with another error returns Err.
        let unauth = dir.path().join("antigravity-unauth");
        std::fs::write(
            &unauth,
            "#!/bin/sh\necho 'Error: authentication required.' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&unauth).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&unauth, perms).unwrap();
        assert_eq!(read_antigravity_cli(&unauth, now).await.unwrap(), None);

        let broken = dir.path().join("antigravity-broken");
        std::fs::write(&broken, "#!/bin/sh\necho 'boom' >&2\nexit 2\n").unwrap();
        let mut perms = std::fs::metadata(&broken).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&broken, perms).unwrap();
        assert!(read_antigravity_cli(&broken, now).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_antigravity_cli_aborts_interactive_oauth_and_enforces_timeout() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let now = at("2026-11-27T12:00:00Z");

        // Interactive OAuth launch on stderr must abort immediately as Ok(None) instead of
        // waiting out the full timeout.
        let oauth_hang = dir.path().join("antigravity-oauth");
        std::fs::write(
            &oauth_hang,
            "#!/bin/sh\necho 'consumerOAuth: starting OAuth flow' >&2\nsleep 30\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&oauth_hang).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&oauth_hang, perms).unwrap();

        let started = std::time::Instant::now();
        let res = read_antigravity_cli_with_timeout(&oauth_hang, now, Duration::from_secs(10))
            .await
            .expect("interactive oauth abort returns Ok(None)");
        assert_eq!(res, None);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "expected immediate abort on interactive OAuth, took {:?}",
            started.elapsed()
        );

        // Silent hang hits the explicit timeout cleanly and kills the child.
        let silent_hang = dir.path().join("antigravity-hang");
        std::fs::write(&silent_hang, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut perms = std::fs::metadata(&silent_hang).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&silent_hang, perms).unwrap();

        let started = std::time::Instant::now();
        let err = read_antigravity_cli_with_timeout(&silent_hang, now, Duration::from_millis(120))
            .await
            .expect_err("silent hang should time out");
        assert!(
            format!("{err:#}").contains("timed out"),
            "unexpected error: {err:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout cleanup took too long: {:?}",
            started.elapsed()
        );
    }
}
