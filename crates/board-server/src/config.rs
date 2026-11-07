use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use board_proto::BoardSlug;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Environment variable that supplies the Telegram bot token, overriding the config file.
pub const TELEGRAM_TOKEN_ENV: &str = "BOARD_TELEGRAM_TOKEN";

/// Server configuration, usually read from a TOML file:
///
/// ```toml
/// db = "/var/lib/agent-board/board.db"
/// listen = "127.0.0.1:7377"
/// users = ["user@example.com"]
/// public_url = "https://board.example.ts.net"
///
/// [telegram]
/// chat_id = 123456789          # optional; else paired with `board admin telegram-pair`
///
/// [defaults]
/// owner_stale_after_secs = 86400
/// ```
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// SQLite database file; created and migrated on start.
    pub db: PathBuf,
    /// Address to listen on. Keep it on loopback and publish it with `tailscale serve`.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Tailscale logins (`Tailscale-User-Login`) treated as the user.
    #[serde(default)]
    pub users: Vec<String>,
    /// Trust the `Tailscale-User-Login` header. Safe only when the server is reachable
    /// solely through `tailscale serve`, which sets that header and strips any sent by the
    /// client; anyone who can reach the listener directly could forge it.
    #[serde(default = "default_true")]
    pub trust_identity_header: bool,
    /// Base URL of the web view, for links in notifications (no trailing slash needed).
    #[serde(default)]
    pub public_url: Option<String>,
    /// Telegram notifier settings. The notifier runs whenever a token is set
    /// (`BOARD_TELEGRAM_TOKEN`), with or without this table.
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    /// Overrides of the rule and rendering defaults (`docs/API.md`, "Defaults").
    #[serde(default)]
    pub defaults: Settings,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7377))
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    /// A configuration with every default, storing the database at `db`.
    pub fn new(db: impl Into<PathBuf>) -> Self {
        Self {
            db: db.into(),
            listen: default_listen(),
            users: Vec::new(),
            trust_identity_header: true,
            public_url: None,
            telegram: None,
            defaults: Settings::default(),
        }
    }

    /// Parses and checks a TOML configuration.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let config: Self =
            toml::from_str(text).map_err(|e| Error::Config(format!("invalid config: {e}")))?;
        config.check()?;
        Ok(config)
    }

    /// Reads and checks a TOML configuration file.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Self::from_toml(&text)
    }

    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.users.iter().any(|u| u.trim().is_empty()) {
            return Err(Error::Config("users must not contain empty logins".into()));
        }
        if let Some(url) = &self.public_url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(Error::Config(
                "public_url must start with http:// or https://".into(),
            ));
        }
        if let Some(telegram) = &self.telegram
            && !(telegram.api_base.starts_with("http://")
                || telegram.api_base.starts_with("https://"))
        {
            return Err(Error::Config(
                "telegram.api_base must start with http:// or https://".into(),
            ));
        }
        self.defaults.check()
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("db", &self.db)
            .field("listen", &self.listen)
            .field("users", &self.users)
            .field("trust_identity_header", &self.trust_identity_header)
            .field("public_url", &self.public_url)
            .field("telegram", &self.telegram)
            .field("defaults", &self.defaults)
            .finish()
    }
}

/// Telegram notifier settings. The token is a secret: it is read from
/// `BOARD_TELEGRAM_TOKEN` when set, otherwise from `bot_token`, and never logged.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: Option<String>,
    /// The only chat that receives asks and whose answers are accepted. When absent, the
    /// chat paired with a `board admin telegram-pair` code is used.
    #[serde(default)]
    pub chat_id: Option<i64>,
    /// Bot API base URL; tests point it at a local fake.
    #[serde(default = "default_telegram_api")]
    pub api_base: String,
}

fn default_telegram_api() -> String {
    "https://api.telegram.org".to_owned()
}

impl Default for TelegramConfig {
    /// No token and no chat: the chat is paired.
    fn default() -> Self {
        Self {
            bot_token: None,
            chat_id: None,
            api_base: default_telegram_api(),
        }
    }
}

impl TelegramConfig {
    /// Settings with a configured chat.
    pub fn new(chat_id: i64) -> Self {
        Self {
            chat_id: Some(chat_id),
            ..Self::default()
        }
    }

    /// The bot token: `BOARD_TELEGRAM_TOKEN` if set and not empty, else `bot_token`.
    pub fn token(&self) -> Option<String> {
        std::env::var(TELEGRAM_TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.bot_token.clone().filter(|t| !t.trim().is_empty()))
    }
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field("chat_id", &self.chat_id)
            .field("api_base", &self.api_base)
            .finish()
    }
}

/// Rule and rendering defaults (`docs/API.md`, "Defaults"); every one can be overridden in
/// the `[defaults]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// An owner unseen this long is stale and its tasks may be taken.
    pub owner_stale_after_secs: u64,
    /// File lease TTL when the request names none; heartbeats renew to at least this.
    pub lease_ttl_secs: u64,
    /// Coordinator lease TTL, renewed by the daemon and the coordinator's heartbeats.
    pub coordinator_ttl_secs: u64,
    /// Touches and leases this recent count as overlapping.
    pub touch_window_secs: u64,
    /// Tasks one agent may hold in `doing` at once.
    pub max_doing_per_agent: u32,
    /// Done tasks older than this are left out of snapshots.
    pub done_hidden_after_days: u32,
    /// A temporary board idle this long (no event on it, no open ask, no live owner of a
    /// task in `doing`) is closed.
    pub temporary_close_after_secs: u64,
    /// A closed temporary board idle this long is deleted.
    pub temporary_delete_after_secs: u64,
    pub snapshot_full_chars: usize,
    pub snapshot_delta_chars: usize,
    pub snapshot_alerts_chars: usize,
    /// Ready tasks listed in a full snapshot.
    pub snapshot_ready_items: usize,
    /// Recent events listed in a full snapshot.
    pub snapshot_recent_items: usize,
    /// Peers listed in a full snapshot.
    pub snapshot_peer_items: usize,
    pub title_chars: usize,
    pub note_chars: usize,
    pub handoff_field_chars: usize,
    pub question_chars: usize,
    pub body_chars: usize,
    /// Board that feedback items are filed on; created on first use.
    pub feedback_board: BoardSlug,
    /// API errors with one signature file feedback once they reach this many...
    pub signal_min_reports: u32,
    /// ...from at least this many agents...
    pub signal_min_agents: u32,
    /// ...within this many days. Older error records are pruned.
    pub signal_window_days: u32,
    /// Creating an ask warns of the board's other open asks younger than this.
    pub duplicate_ask_window_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            owner_stale_after_secs: 24 * 3600,
            lease_ttl_secs: 45 * 60,
            coordinator_ttl_secs: 45 * 60,
            touch_window_secs: 3600,
            max_doing_per_agent: 1,
            done_hidden_after_days: 30,
            temporary_close_after_secs: 24 * 3600,
            temporary_delete_after_secs: 7 * 24 * 3600,
            snapshot_full_chars: 2000,
            snapshot_delta_chars: 600,
            snapshot_alerts_chars: 400,
            snapshot_ready_items: 3,
            snapshot_recent_items: 3,
            snapshot_peer_items: 8,
            title_chars: 72,
            note_chars: 280,
            handoff_field_chars: 200,
            question_chars: 1000,
            body_chars: 4000,
            feedback_board: "board-feedback".parse().expect("a valid slug"),
            signal_min_reports: 3,
            signal_min_agents: 2,
            signal_window_days: 7,
            duplicate_ask_window_secs: 30 * 60,
        }
    }
}

impl Settings {
    fn check(&self) -> Result<(), Error> {
        let positive = [
            ("owner_stale_after_secs", self.owner_stale_after_secs),
            ("lease_ttl_secs", self.lease_ttl_secs),
            ("coordinator_ttl_secs", self.coordinator_ttl_secs),
            ("touch_window_secs", self.touch_window_secs),
            ("max_doing_per_agent", u64::from(self.max_doing_per_agent)),
            (
                "temporary_close_after_secs",
                self.temporary_close_after_secs,
            ),
            (
                "temporary_delete_after_secs",
                self.temporary_delete_after_secs,
            ),
            ("title_chars", self.title_chars as u64),
            ("note_chars", self.note_chars as u64),
            ("handoff_field_chars", self.handoff_field_chars as u64),
            ("question_chars", self.question_chars as u64),
            ("body_chars", self.body_chars as u64),
            ("signal_min_reports", u64::from(self.signal_min_reports)),
            ("signal_min_agents", u64::from(self.signal_min_agents)),
            ("signal_window_days", u64::from(self.signal_window_days)),
            ("duplicate_ask_window_secs", self.duplicate_ask_window_secs),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return Err(Error::Config(format!("defaults.{name} must be positive")));
        }
        if self.snapshot_full_chars < 600 {
            return Err(Error::Config(
                "defaults.snapshot_full_chars must be at least 600".into(),
            ));
        }
        if self.snapshot_delta_chars < 200 || self.snapshot_alerts_chars < 200 {
            return Err(Error::Config(
                "defaults.snapshot_delta_chars and snapshot_alerts_chars must be at least 200"
                    .into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn stale_ms(&self) -> i64 {
        secs_ms(self.owner_stale_after_secs)
    }

    pub(crate) fn lease_ms(&self) -> i64 {
        secs_ms(self.lease_ttl_secs)
    }

    pub(crate) fn coordinator_ms(&self) -> i64 {
        secs_ms(self.coordinator_ttl_secs)
    }

    pub(crate) fn temporary_close_ms(&self) -> i64 {
        secs_ms(self.temporary_close_after_secs)
    }

    pub(crate) fn temporary_delete_ms(&self) -> i64 {
        secs_ms(self.temporary_delete_after_secs)
    }

    pub(crate) fn touch_ms(&self) -> i64 {
        secs_ms(self.touch_window_secs)
    }

    pub(crate) fn signal_window_ms(&self) -> i64 {
        secs_ms(u64::from(self.signal_window_days) * 86_400)
    }

    pub(crate) fn done_hidden_ms(&self) -> i64 {
        secs_ms(u64::from(self.done_hidden_after_days) * 86_400)
    }
}

fn secs_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_and_full_configs() {
        let minimal = ServerConfig::from_toml("db = \"board.db\"").unwrap();
        assert_eq!(minimal.listen.to_string(), "127.0.0.1:7377");
        assert!(minimal.trust_identity_header);
        assert_eq!(minimal.defaults, Settings::default());

        let full = ServerConfig::from_toml(
            r#"
            db = "b.db"
            listen = "127.0.0.1:9000"
            users = ["user@example.com"]
            trust_identity_header = false
            public_url = "https://board.example.ts.net"
            [telegram]
            bot_token = "secret-token"
            chat_id = -100
            [defaults]
            owner_stale_after_secs = 60
            snapshot_full_chars = 1500
            feedback_board = "setup-feedback"
            "#,
        )
        .unwrap();
        assert_eq!(full.defaults.owner_stale_after_secs, 60);
        assert_eq!(full.defaults.snapshot_full_chars, 1500);
        assert_eq!(full.defaults.feedback_board.as_str(), "setup-feedback");
        assert_eq!(minimal.defaults.feedback_board.as_str(), "board-feedback");
        assert_eq!(
            full.defaults.lease_ttl_secs,
            Settings::default().lease_ttl_secs
        );
        assert_eq!(full.telegram.as_ref().unwrap().chat_id, Some(-100));
        let paired = ServerConfig::from_toml("db = \"b\"\n[telegram]\n").unwrap();
        assert_eq!(paired.telegram.unwrap().chat_id, None);
        assert!(!format!("{full:?}").contains("secret-token"));
    }

    #[test]
    fn rejects_unknown_and_bad_values() {
        assert!(ServerConfig::from_toml("db = \"b\"\nlisten_addr = \"x\"").is_err());
        assert!(
            ServerConfig::from_toml("db = \"b\"\n[defaults]\nmax_doing_per_agent = 0").is_err()
        );
        assert!(ServerConfig::from_toml("db = \"b\"\npublic_url = \"board\"").is_err());
        assert!(
            ServerConfig::from_toml("db = \"b\"\n[defaults]\nfeedback_board = \"Bad Slug\"")
                .is_err()
        );
        for name in [
            "temporary_close_after_secs",
            "temporary_delete_after_secs",
            "signal_min_reports",
            "signal_min_agents",
            "signal_window_days",
        ] {
            assert!(
                ServerConfig::from_toml(&format!("db = \"b\"\n[defaults]\n{name} = 0")).is_err(),
                "{name}"
            );
            let set =
                ServerConfig::from_toml(&format!("db = \"b\"\n[defaults]\n{name} = 60")).unwrap();
            assert_ne!(set.defaults, Settings::default(), "{name}");
        }
    }
}
