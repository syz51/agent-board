//! The event feed: a JSON page, or a server-sent event stream that resumes from
//! `Last-Event-ID`.

use std::convert::Infallible;

use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use board_proto::{BoardSlug, Event, EventsPage};
use futures_util::{Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::db::Ctx;
use crate::error::Result;
use crate::state::AppState;
use crate::store::{NamedParams, load_events};

/// Events per page when the request names no limit, and the most it may ask for.
pub(crate) const DEFAULT_LIMIT: u32 = 100;
pub(crate) const MAX_LIMIT: u32 = 1000;

pub(crate) fn page(
    cx: &Ctx<'_>,
    since: i64,
    board: Option<&BoardSlug>,
    limit: u32,
) -> Result<EventsPage> {
    let events = match board {
        Some(board) => {
            let b = board.as_str();
            let params: NamedParams<'_> = vec![(":board", &b)];
            load_events(cx, "board = :board", &params, since, limit)?
        }
        None => load_events(cx, "1 = 1", &Vec::new(), since, limit)?,
    };
    let next_since = events.last().map_or(since, |e| e.seq);
    Ok(EventsPage { events, next_since })
}

fn to_sse(event: &Event) -> SseEvent {
    SseEvent::default()
        .id(event.seq.to_string())
        .data(serde_json::to_string(event).expect("events serialize"))
}

/// Streams events after `since`: first the backlog from the database, then live events as
/// they commit. Ends when the client goes away or the server shuts down.
pub(crate) fn stream(
    state: AppState,
    since: i64,
    board: Option<BoardSlug>,
) -> Sse<impl Stream<Item = std::result::Result<SseEvent, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Event>(256);
    // Subscribe before reading the backlog so nothing committed in between is missed;
    // duplicates are dropped by sequence number.
    let mut live = state.db().subscribe();
    tokio::spawn(async move {
        let mut last = since;
        if !send_backlog(&state, &tx, &mut last, board.as_ref()).await {
            return;
        }
        loop {
            tokio::select! {
                () = state.stopped() => return,
                () = tx.closed() => return,
                received = live.recv() => match received {
                    Ok(event) => {
                        if event.seq <= last {
                            continue;
                        }
                        last = event.seq;
                        let wanted = board.as_ref().is_none_or(|b| event.board.as_ref() == Some(b));
                        if wanted && tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if !send_backlog(&state, &tx, &mut last, board.as_ref()).await {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    });
    Sse::new(ReceiverStream::new(rx).map(|event| Ok(to_sse(&event))))
        .keep_alive(KeepAlive::default())
}

/// Sends stored events after `last`; false when the stream should end.
async fn send_backlog(
    state: &AppState,
    tx: &mpsc::Sender<Event>,
    last: &mut i64,
    board: Option<&BoardSlug>,
) -> bool {
    loop {
        let since = *last;
        let board_owned = board.cloned();
        let page = match state
            .read(move |cx| page(cx, since, board_owned.as_ref(), MAX_LIMIT))
            .await
        {
            Ok(page) => page,
            Err(error) => {
                tracing::error!(%error, "event stream backlog failed");
                return false;
            }
        };
        let full = page.events.len() as u32 == MAX_LIMIT;
        for event in page.events {
            *last = event.seq;
            if tx.send(event).await.is_err() {
                return false;
            }
        }
        if !full {
            return true;
        }
    }
}
