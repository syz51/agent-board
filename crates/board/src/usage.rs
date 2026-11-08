//! Reads the account's usage from the endpoint Claude Code's `/usage` shows, with Claude
//! Code's own sign-in, for the daemon to report (`docs/DESIGN.md`, "Usage").
//!
//! The token is only read: never refreshed, which could sign Claude Code out, and never
//! logged or sent anywhere but the usage endpoint. An expired token skips the reading until
//! Claude Code refreshes it.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use board_proto::{ReportUsage, Timestamp, UsageWindow};
use serde::Deserialize;

/// Claude Code's usage endpoint.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The beta the endpoint asks for.
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The keychain item Claude Code keeps its sign-in in, on macOS.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Reading the sign-in and the endpoint each get this long.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the daemon gets the account's usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageSource {
    /// Reads nothing itself; uses the readings other devices report to the server.
    Server,
    /// Reads `url` with the token from `credentials`, and reports the reading.
    ClaudeCode {
        url: String,
        credentials: Credentials,
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
    /// without a home directory to find it in.
    pub fn claude_code() -> Self {
        let credentials = if cfg!(target_os = "macos") {
            Credentials::Keychain
        } else {
            match dirs::home_dir() {
                Some(home) => Credentials::File(home.join(".claude/.credentials.json")),
                None => return Self::Server,
            }
        };
        Self::ClaudeCode {
            url: USAGE_URL.to_owned(),
            credentials,
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
