//! agent-board server: storage, rules, the `/v1` HTTP API with its event stream,
//! snapshot rendering, the embedded web view and the Telegram notifier.
//!
//! The binary (`board serve`, `board admin`) uses [`serve`], [`create_device`],
//! [`telegram_pair`] and [`backup`]; tests build [`app`] over [`Db::open_in_memory`] with a [`ManualClock`].

mod api;
mod auth;
mod clock;
mod config;
mod db;
mod error;
mod events;
mod ops;
mod snapshot;
mod state;
mod store;
mod telegram;
mod text;
mod web;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use board_proto::DeviceName;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{ServerConfig, Settings, TELEGRAM_TOKEN_ENV, TelegramConfig};
pub use db::Db;
pub use error::Error;
pub use ops::devices::DeviceInfo;
pub use state::AppState;
pub use telegram::spawn_telegram;
pub use text::frame_peer_text;

/// How often expired leases are swept (and `lease_expired` recorded).
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The HTTP application: `/healthz`, the `/v1` API and the web view.
pub fn app(state: AppState) -> axum::Router {
    api::router(state)
}

/// Runs the server until SIGTERM or Ctrl-C, then shuts down gracefully: event streams end,
/// in-flight requests finish, background tasks stop.
pub async fn serve(config: ServerConfig) -> Result<(), Error> {
    config.check()?;
    let db = Db::open(&config.db)?;
    let state = AppState::new(db, &config, Arc::new(SystemClock));
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(addr = %config.listen, "board server listening");
    let mut tasks = vec![spawn_sweeper(state.clone())];
    // A token alone starts the notifier; the chat is then paired (`board admin telegram-pair`).
    let telegram = config.telegram.clone().unwrap_or_default();
    match spawn_telegram(state.clone(), telegram)? {
        Some(task) => tasks.push(task),
        None if config.telegram.is_some() => tracing::warn!(
            "telegram is configured without a token; set {TELEGRAM_TOKEN_ENV} or bot_token"
        ),
        None => {}
    }
    let stopping = state.clone();
    let served = axum::serve(listener, app(state.clone()))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            stopping.shutdown();
        })
        .await;
    state.shutdown();
    for task in tasks {
        let _ = task.await;
    }
    served.map_err(Error::from)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// Sweeps expired leases every minute until shutdown.
pub fn spawn_sweeper(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tokio::select! {
                () = state.stopped() => return,
                _ = tick.tick() => {
                    if let Err(error) = state.write(ops::leases::sweep_expired).await {
                        tracing::error!(%error, "lease sweep failed");
                    }
                }
            }
        }
    })
}

/// Creates a device (a user device when `user`) in the database at `db_path` and returns
/// its token, shown once. For `board admin` on the host.
pub fn create_device(db_path: &Path, name: &DeviceName, user: bool) -> Result<String, Error> {
    let db = Db::open(db_path)?;
    db.create_device(name, user, SystemClock.now())
}

/// Creates a one-time Telegram pairing code in the database at `db_path` and returns it,
/// shown once; with `replace` the code may take over from a paired chat. For `board admin` on the host.
pub fn telegram_pair(db_path: &Path, replace: bool) -> Result<String, Error> {
    let db = Db::open(db_path)?;
    db.create_telegram_pairing(replace, SystemClock.now())
}

/// Writes a consistent copy of the database at `db_path` to `out_path` (`VACUUM INTO`),
/// which must not exist yet.
pub fn backup(db_path: &Path, out_path: &Path) -> Result<(), Error> {
    if out_path.exists() {
        return Err(Error::Config(format!(
            "{} already exists; choose a new backup path",
            out_path.display()
        )));
    }
    let out = out_path
        .to_str()
        .ok_or_else(|| Error::Config(format!("backup path {} is not UTF-8", out_path.display())))?;
    let conn = db::open_file(db_path)?;
    conn.execute("VACUUM INTO ?1", [out])?;
    Ok(())
}
