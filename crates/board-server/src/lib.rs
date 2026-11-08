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

use board_proto::{BoardSlug, DeviceName};
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

/// Every minute until shutdown: sweeps expired leases, prunes old API error records,
/// announces passed `not_before` times, quiet reviews and owners away from their `doing`
/// tasks in one transaction, then closes or
/// deletes idle temporary boards, one transaction per board, then prunes history past the
/// stats window in batches.
pub fn spawn_sweeper(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tokio::select! {
                () = state.stopped() => return,
                _ = tick.tick() => {
                    let swept = state
                        .write(|cx| {
                            ops::leases::sweep_expired(cx)?;
                            ops::feedback::prune_signals(cx)?;
                            ops::tasks::announce_not_before(cx)?;
                            ops::tasks::announce_quiet_reviews(cx)?;
                            ops::tasks::announce_idle_doing(cx)
                        })
                        .await;
                    if let Err(error) = swept {
                        tracing::error!(%error, "sweep failed");
                    }
                    sweep_temporary(&state).await;
                    if let Err(error) = prune_history(&state).await {
                        tracing::error!(%error, "pruning old history failed");
                    }
                }
            }
        }
    })
}

/// Batches of history one sweep prunes at most; a larger backlog waits for the next sweep.
const PRUNE_BATCHES: usize = 100;

/// Deletes events and refused claims past the stats window, one batch per transaction so
/// other writes go between them, until a batch finds none due or the server stops. Returns
/// how many events and refused claims went.
pub(crate) async fn prune_history(state: &AppState) -> Result<(usize, usize), Error> {
    let mut pruned = (0, 0);
    for _ in 0..PRUNE_BATCHES {
        if futures_util::FutureExt::now_or_never(state.stopped()).is_some() {
            break;
        }
        let (events, refusals) = state.write(|cx| ops::stats::prune_history(cx)).await?;
        pruned = (pruned.0 + events, pruned.1 + refusals);
        // An event batch can come back short while due events remain behind one dated
        // later (a clock that was ahead), so only an empty one ends the pass.
        if events == 0 && refusals < ops::stats::PRUNE_BATCH {
            break;
        }
    }
    Ok(pruned)
}

/// What one sweep of temporary boards did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TemporarySweep {
    pub closed: Vec<BoardSlug>,
    pub deleted: Vec<BoardSlug>,
    /// Boards whose transaction failed; logged, and tried again on the next sweep.
    pub failed: Vec<BoardSlug>,
}

/// Closes or deletes the temporary boards that are due, each in its own transaction.
pub(crate) async fn sweep_temporary(state: &AppState) -> TemporarySweep {
    use ops::boards::Swept;
    let since = state.started().timestamp_millis();
    let mut out = TemporarySweep::default();
    let due = match state
        .read(move |cx| ops::boards::temporary_due(cx, since))
        .await
    {
        Ok(due) => due,
        Err(error) => {
            tracing::error!(%error, "temporary board sweep failed");
            return out;
        }
    };
    for slug in due {
        let board = slug.clone();
        match state
            .write(move |cx| ops::boards::sweep_temporary(cx, &board, since))
            .await
        {
            Ok(Some(Swept::Closed)) => {
                tracing::info!(board = %slug, "closed idle temporary board");
                out.closed.push(slug);
            }
            Ok(Some(Swept::Deleted)) => {
                tracing::info!(board = %slug, "deleted idle temporary board");
                out.deleted.push(slug);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(board = %slug, %error, "temporary board sweep failed");
                out.failed.push(slug);
            }
        }
    }
    out
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

/// Writes a consistent copy of the database at `db_path`, which must exist, to `out_path`
/// (`VACUUM INTO`), which must not exist yet.
pub fn backup(db_path: &Path, out_path: &Path) -> Result<(), Error> {
    // Opening a missing file would create an empty database and back that up.
    if !db_path.is_file() {
        return Err(Error::Config(format!(
            "{} is not a database file; pass the server's database (`db` in server.toml)",
            db_path.display()
        )));
    }
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
