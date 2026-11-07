use std::sync::Arc;

use board_proto::Timestamp;
use tokio::sync::watch;

use crate::clock::{Clock, now_ms};
use crate::config::{ServerConfig, Settings};
use crate::db::{Ctx, Db};
use crate::error::{Error, Result};

/// Everything a request handler needs: the database, the clock and the settings.
/// Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    clock: Arc<dyn Clock>,
    /// When this server started: the temporary-board sweep counts no idle time before it.
    started: Timestamp,
    settings: Settings,
    users: Vec<String>,
    trust_identity_header: bool,
    public_url: Option<String>,
    shutdown: watch::Sender<bool>,
}

impl AppState {
    /// Builds the state over an open database. Tests pass an in-memory database and a
    /// [`crate::ManualClock`].
    pub fn new(db: Db, config: &ServerConfig, clock: Arc<dyn Clock>) -> Self {
        let (shutdown, _) = watch::channel(false);
        let started = now_ms(clock.as_ref());
        Self {
            inner: Arc::new(Inner {
                db,
                clock,
                started,
                settings: config.defaults.clone(),
                users: config.users.clone(),
                trust_identity_header: config.trust_identity_header,
                public_url: config
                    .public_url
                    .as_deref()
                    .map(|u| u.trim_end_matches('/').to_owned()),
                shutdown,
            }),
        }
    }

    /// The current time, truncated to the stored precision.
    pub fn now(&self) -> Timestamp {
        now_ms(self.inner.clock.as_ref())
    }

    /// When this server started, on its clock.
    pub(crate) fn started(&self) -> Timestamp {
        self.inner.started
    }

    /// Creates a device (a user device when `user`) and returns its token, shown once.
    pub fn create_device(&self, name: &board_proto::DeviceName, user: bool) -> Result<String> {
        self.inner.db.create_device(name, user, self.now())
    }

    /// Creates a one-time Telegram pairing code and returns it, shown once.
    pub fn create_telegram_pairing(&self, replace: bool) -> Result<String> {
        self.inner.db.create_telegram_pairing(replace, self.now())
    }

    /// Ends event streams and background tasks; called on server shutdown.
    pub fn shutdown(&self) {
        self.inner.shutdown.send_replace(true);
    }

    /// Resolves once [`AppState::shutdown`] has been called.
    pub(crate) async fn stopped(&self) {
        let mut rx = self.inner.shutdown.subscribe();
        // Only fails when the sender is gone, which cannot happen while `self` lives.
        let _ = rx.wait_for(|stop| *stop).await.map(|_| ());
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.db
    }

    pub(crate) fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    pub(crate) fn is_user_login(&self, login: &str) -> bool {
        self.inner.trust_identity_header && self.inner.users.iter().any(|u| u == login)
    }

    pub(crate) fn trusts_identity_header(&self) -> bool {
        self.inner.trust_identity_header
    }

    pub(crate) fn public_url(&self) -> Option<&str> {
        self.inner.public_url.as_deref()
    }

    /// Runs a write transaction on a blocking thread.
    pub(crate) async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Ctx<'_>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            state
                .inner
                .db
                .write(|| state.now(), &state.inner.settings, f)
        })
        .await
        .map_err(|e| Error::Internal(format!("write task failed: {e}")))?
    }

    /// Runs a read transaction on a blocking thread.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Ctx<'_>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let now = state.now();
            state.inner.db.read(now, &state.inner.settings, f)
        })
        .await
        .map_err(|e| Error::Internal(format!("read task failed: {e}")))?
    }
}
