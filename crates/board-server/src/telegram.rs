//! Telegram notifier: sends each new ask to the chat, with its options as buttons, and
//! long-polls the Bot API for answers (a button press, or a reply to the ask's message)
//! from that chat only. A withdrawn ask's message is edited to say so and loses its
//! buttons. No public endpoint is needed.
//!
//! Every minute it also reports tasks in doing or review that went quiet past
//! `stuck_after_secs`, in one message per check and once until they change; reports active
//! autopilot boards whose ready work sat unclaimed past `stalled_after_secs`, once until
//! the board stalls anew; and sends a daily Needs you digest at
//! `digest_at` (skipped when there is nothing to list). Each is recorded only once sent, so
//! an alert is never lost; a crash between the send and the record repeats it once.
//!
//! The chat is `[telegram] chat_id` when configured, else the one paired in the database:
//! the first private chat that sends `/start <code>` with a live code from `board admin
//! telegram-pair` becomes it (a `--replace` code takes over from a paired chat). Asks
//! created while no chat is paired are sent when one is, with every other open ask.
//!
//! The bot token is part of every Bot API URL, so request errors are logged without their
//! URL and the token never reaches the log.

use std::time::Duration;

use board_proto::{AgentRole, AskStatus, Event, EventType, Timestamp};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeDelta};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::config::TelegramConfig;
use crate::error::{Error, Result};
use crate::events;
use crate::ops::asks;
use crate::ops::telegram as telegram_ops;
use crate::state::AppState;
use crate::store::{find_task, get_agent, get_ask, get_board};

/// Long-poll timeout passed to `getUpdates`.
const POLL_SECS: u64 = 25;
const RETRY: Duration = Duration::from_secs(5);
/// How often the scheduled alerts are checked.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// Room for the digest, stuck and stalled texts: Telegram takes at most 4,096 characters per message.
const DIGEST_CHARS: usize = 3_500;
/// How long after its time a day's digest still goes out once the local date has turned.
const DIGEST_GRACE_MINS: i64 = 60;
/// Longest ask question or task title in the digest, in characters.
const DIGEST_LINE_CHARS: usize = 140;

/// When the scheduled alerts go out, from `[telegram]`.
#[derive(Debug, Clone, Copy)]
struct Schedule {
    /// 0 turns the stuck alert off.
    stuck_after_secs: u64,
    /// 0 turns the stalled-board alert off.
    stalled_after_secs: u64,
    digest_at: Option<NaiveTime>,
    utc_offset: FixedOffset,
}

#[derive(Clone)]
struct Bot {
    http: reqwest::Client,
    /// `<api_base>/bot<token>`: secret, never logged.
    base: String,
    /// `[telegram] chat_id`, which wins over a paired chat.
    configured: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Reply<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    text: Option<String>,
    reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    message: Option<Message>,
    data: Option<String>,
}

impl Bot {
    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: &Value) -> Result<T> {
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("telegram {method}: {}", e.without_url())))?;
        let reply: Reply<T> = response
            .json()
            .await
            .map_err(|e| Error::Internal(format!("telegram {method}: {}", e.without_url())))?;
        match reply {
            Reply {
                ok: true,
                result: Some(result),
                ..
            } => Ok(result),
            Reply { description, .. } => Err(Error::Internal(format!(
                "telegram {method} failed: {}",
                description.unwrap_or_else(|| "no description".into())
            ))),
        }
    }

    async fn send(&self, body: Value) -> Result<Message> {
        self.call("sendMessage", &body).await
    }

    /// The chat to notify and accept answers from: the configured one, else the paired one.
    async fn chat(&self, state: &AppState) -> Result<Option<i64>> {
        if let Some(chat) = self.configured {
            return Ok(Some(chat));
        }
        state.read(telegram_ops::paired_chat).await
    }
}

/// Starts the notifier when a token is configured (`BOARD_TELEGRAM_TOKEN` or
/// `bot_token`); `Ok(None)` otherwise. Runs until the state shuts down. Events committed
/// before it starts are not notified.
pub fn spawn_telegram(state: AppState, config: TelegramConfig) -> Result<Option<JoinHandle<()>>> {
    // Subscribe first, so every event after `since` is either received or replayed.
    let events = state.db().subscribe();
    let since = state
        .db()
        .read(state.now(), state.settings(), |cx| cx.max_seq())?;
    spawn_with(state, config, events, since, CHECK_EVERY, None)
}

/// [`spawn_telegram`] notifying from `events`, starting after event `since`, checking the
/// scheduled alerts every `check_every` and counting finished checks in `checked`; tests
/// pass a small channel here to make the notifier lag, and a short period.
pub(crate) fn spawn_with(
    state: AppState,
    config: TelegramConfig,
    events: broadcast::Receiver<Event>,
    since: i64,
    check_every: Duration,
    checked: Option<watch::Sender<u64>>,
) -> Result<Option<JoinHandle<()>>> {
    let Some(token) = config.token() else {
        return Ok(None);
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(POLL_SECS + 15))
        .build()
        .map_err(|e| Error::Internal(format!("telegram client: {}", e.without_url())))?;
    let bot = Bot {
        http,
        base: format!("{}/bot{token}", config.api_base.trim_end_matches('/')),
        configured: config.chat_id,
    };
    if config.chat_id.is_none()
        && state
            .db()
            .read(state.now(), state.settings(), telegram_ops::paired_chat)?
            .is_none()
    {
        tracing::info!(
            "telegram: no chat yet; run `board admin telegram-pair --db <file>` on the host \
             and send the bot `/start <code>` in a private chat"
        );
    }
    let schedule = Schedule {
        stuck_after_secs: config.stuck_after_secs,
        stalled_after_secs: config.stalled_after_secs,
        digest_at: config.digest_at,
        utc_offset: config.utc_offset,
    };
    let notify = tokio::spawn(notify_loop(state.clone(), bot.clone(), events, since));
    // With every alert off there is nothing to check.
    let alerts = (schedule.stuck_after_secs > 0
        || schedule.stalled_after_secs > 0
        || schedule.digest_at.is_some())
    .then(|| {
        tokio::spawn(alert_loop(
            state.clone(),
            bot.clone(),
            schedule,
            check_every,
            checked,
        ))
    });
    let poll = tokio::spawn(poll_loop(state, bot));
    Ok(Some(tokio::spawn(async move {
        let alerts = async {
            if let Some(alerts) = alerts {
                let _ = alerts.await;
            }
        };
        let _ = tokio::join!(notify, alerts, poll);
    })))
}

/// Notifies each event after `last` once. When the live feed lags, the missed events are
/// replayed from the database, as the SSE stream does.
async fn notify_loop(
    state: AppState,
    bot: Bot,
    mut events: broadcast::Receiver<Event>,
    mut last: i64,
) {
    loop {
        let received = tokio::select! {
            () = state.stopped() => return,
            received = events.recv() => received,
        };
        match received {
            Ok(event) => {
                if event.seq <= last {
                    continue;
                }
                last = event.seq;
                notify(&state, &bot, &event).await;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "telegram notifier fell behind the event feed; replaying"
                );
                if let Err(error) = replay(&state, &bot, &mut last).await {
                    tracing::error!(%error, after = last, "telegram replay failed");
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Notifies the stored events after `last`, advancing it.
async fn replay(state: &AppState, bot: &Bot, last: &mut i64) -> Result<()> {
    loop {
        let since = *last;
        let page = state
            .read(move |cx| events::page(cx, since, None, events::MAX_LIMIT))
            .await?;
        let full = page.events.len() == events::MAX_LIMIT as usize;
        for event in &page.events {
            *last = event.seq;
            notify(state, bot, event).await;
        }
        if !full {
            return Ok(());
        }
    }
}

async fn notify(state: &AppState, bot: &Bot, event: &Event) {
    let result = match event.event_type {
        EventType::AskCreated => send_ask(state, bot, event).await,
        EventType::AskWithdrawn => mark_withdrawn(state, bot, event).await,
        EventType::AgentStateChanged => report_paused(state, bot, event).await,
        _ => Ok(()),
    };
    if let Err(error) = result {
        tracing::error!(%error, seq = event.seq, "telegram notification failed");
    }
}

async fn send_ask(state: &AppState, bot: &Bot, event: &Event) -> Result<()> {
    let Some(id) = event.payload.get("id").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Some(chat) = bot.chat(state).await? else {
        tracing::warn!(
            ask = id,
            "telegram: no chat paired; the ask is sent once one is"
        );
        return Ok(());
    };
    send_ask_to(state, bot, chat, id).await
}

/// Sends ask `id` to `chat` if it is still open and not yet sent, and records the
/// message, so replies to it answer the ask.
async fn send_ask_to(state: &AppState, bot: &Bot, chat: i64, id: i64) -> Result<()> {
    let (ask, text) = ask_text(state, id).await?;
    let paired = bot.configured.is_none();
    if ask.status != AskStatus::Open
        || !state
            .write(move |cx| asks::claim_telegram_send(cx, id, chat, paired))
            .await?
    {
        return Ok(());
    }
    let mut body = json!({ "chat_id": chat, "text": text });
    if !ask.options.is_empty() {
        let rows: Vec<Value> = ask
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                json!([{ "text": option, "callback_data": format!("ask:{}:{i}", ask.id) }])
            })
            .collect();
        body["reply_markup"] = json!({ "inline_keyboard": rows });
    }
    let message = bot.send(body).await?.message_id;
    let status = state
        .write(move |cx| asks::set_telegram_message(cx, id, chat, message))
        .await?;
    if status == AskStatus::Withdrawn {
        // Withdrawn while it was being sent, when there was no message to mark yet.
        edit_withdrawn(state, bot, id, chat, message).await?;
    }
    Ok(())
}

/// Marks a withdrawn ask's message as withdrawn and drops its buttons, so the user
/// does not answer it.
async fn mark_withdrawn(state: &AppState, bot: &Bot, event: &Event) -> Result<()> {
    let Some(id) = event.payload.get("id").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Some((chat, message)) = state.read(move |cx| asks::telegram_message(cx, id)).await? else {
        return Ok(());
    };
    edit_withdrawn(state, bot, id, chat, message).await
}

async fn edit_withdrawn(
    state: &AppState,
    bot: &Bot,
    id: i64,
    chat: i64,
    message: i64,
) -> Result<()> {
    let (_, text) = ask_text(state, id).await?;
    bot.call::<Value>(
        "editMessageText",
        &json!({ "chat_id": chat, "message_id": message, "text": text }),
    )
    .await?;
    Ok(())
}

/// Ask `id` and the text of its Telegram message, which tells the user how to answer it
/// or that it was withdrawn.
async fn ask_text(state: &AppState, id: i64) -> Result<(board_proto::Ask, String)> {
    let (ask, board, task, handle) = state
        .read(move |cx| {
            let ask = get_ask(cx, id)?;
            let board = get_board(cx, &ask.board)?;
            let task = ask
                .task
                .as_ref()
                .map(|t| find_task(cx, t))
                .transpose()?
                .flatten();
            let handle = get_agent(cx, &ask.agent)?.handle;
            Ok((ask, board, task, handle))
        })
        .await?;
    let kind = match ask.kind {
        board_proto::AskKind::Decision => "decision",
        board_proto::AskKind::Action => "action",
    };
    let mut text = format!(
        "Board {} · ask {} ({kind}) from {handle}\n",
        board.slug, ask.id
    );
    if let Some(task) = &task {
        text.push_str(&format!("Task {}: {}\n", task.id, task.title));
    }
    text.push('\n');
    text.push_str(&ask.question);
    text.push_str("\n\n");
    if ask.status == AskStatus::Withdrawn {
        text.push_str("Withdrawn by the agent; no answer is needed.");
    } else if ask.options.is_empty() {
        text.push_str("Reply to this message to answer.");
    } else {
        text.push_str("Pick an option, or reply to this message to answer freely.");
    }
    if let Some(url) = state.public_url() {
        text.push_str(&format!("\n{url}/asks/{}", ask.id));
    }
    Ok((ask, text))
}

/// Reports a coordinator that stopped on a usage limit.
async fn report_paused(state: &AppState, bot: &Bot, event: &Event) -> Result<()> {
    if event.payload.get("to").and_then(Value::as_str) != Some("paused") {
        return Ok(());
    }
    let Ok(id) = event.actor.parse::<board_proto::AgentId>() else {
        return Ok(());
    };
    let agent = state.read(move |cx| get_agent(cx, &id)).await?;
    if agent.role != AgentRole::Coordinator {
        return Ok(());
    }
    let Some(chat) = bot.chat(state).await? else {
        return Ok(());
    };
    let until = agent
        .paused_until
        .map_or_else(|| "further notice".to_owned(), |t| t.to_rfc3339());
    let boards: Vec<String> = agent.boards.iter().map(ToString::to_string).collect();
    let text = format!(
        "Coordinator {} ({}) is paused until {until}.",
        agent.handle,
        boards.join(", ")
    );
    bot.send(json!({ "chat_id": chat, "text": text }))
        .await
        .map(|_| ())
}

/// Checks the scheduled alerts every `every` until the state shuts down, and
/// counts each finished check in `checked` (tests wait on it).
async fn alert_loop(
    state: AppState,
    bot: Bot,
    schedule: Schedule,
    every: Duration,
    checked: Option<watch::Sender<u64>>,
) {
    // The first check waits a period, so a starting server does not alert before it settles.
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = state.stopped() => return,
            _ = tick.tick() => {}
        }
        // Shutdown drops a check in flight: an alert is recorded only once it is sent, so
        // one cut off is sent after the restart.
        tokio::select! {
            () = state.stopped() => return,
            () = check(&state, &bot, &schedule) => {}
        }
        if let Some(checked) = &checked {
            checked.send_modify(|n| *n += 1);
        }
    }
}

async fn check(state: &AppState, bot: &Bot, schedule: &Schedule) {
    let chat = match bot.chat(state).await {
        Ok(Some(chat)) => chat,
        // Nothing is recorded without a chat, so a chat paired later gets the alerts.
        Ok(None) => return,
        Err(error) => {
            tracing::error!(%error, "telegram: reading the chat failed");
            return;
        }
    };
    if schedule.stuck_after_secs > 0
        && let Err(error) = send_stuck(state, bot, chat, schedule).await
    {
        tracing::error!(%error, "telegram: the stuck-task alert failed; retrying at the next check");
    }
    if schedule.stalled_after_secs > 0
        && let Err(error) = send_stalled(state, bot, chat, schedule).await
    {
        tracing::error!(%error, "telegram: the stalled-board alert failed; retrying at the next check");
    }
    if let Some(at) = schedule.digest_at
        && let Err(error) = send_digest(state, bot, chat, at, schedule.utc_offset).await
    {
        tracing::error!(%error, "telegram: the digest failed; retrying at the next check");
    }
}

/// Reports the tasks that went quiet past the threshold in one message, each once until it
/// changes. Only what was sent is recorded, so a failed send, or tasks past the message's
/// length, go in the next check's message.
async fn send_stuck(state: &AppState, bot: &Bot, chat: i64, schedule: &Schedule) -> Result<()> {
    let quiet_ms =
        i64::try_from(schedule.stuck_after_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let stuck = state
        .read(move |cx| telegram_ops::stuck(cx, quiet_ms))
        .await?;
    if stuck.is_empty() {
        return Ok(());
    }
    let (text, shown) = stuck_text(state, &stuck, schedule);
    bot.send(json!({ "chat_id": chat, "text": text })).await?;
    let sent: Vec<_> = stuck
        .into_iter()
        .take(shown)
        .map(|task| (task.id, task.seq))
        .collect();
    state
        .write(move |cx| telegram_ops::record_stuck(cx, &sent))
        .await
}

/// The stuck message, and how many of `stuck` it lists.
fn stuck_text(
    state: &AppState,
    stuck: &[telegram_ops::Stuck],
    schedule: &Schedule,
) -> (String, usize) {
    let threshold =
        TimeDelta::seconds(i64::try_from(schedule.stuck_after_secs).unwrap_or(i64::MAX));
    let mut text = format!(
        "Stuck: {} {} with no update for {} or more.\n",
        stuck.len(),
        plural(stuck.len(), "task", "tasks"),
        span(threshold),
    );
    let mut shown = 0;
    for task in stuck {
        let owner = task
            .owner
            .as_deref()
            .map_or_else(|| "no owner".to_owned(), |h| format!("owner {h}"));
        let since = local(task.quiet_since, schedule.utc_offset);
        let last = match &task.last {
            Some((kind, actor)) => format!("Last: {kind} by {actor} at {since}."),
            None => format!("Last change at {since}."),
        };
        let mut item = format!(
            "\n{} {}, quiet for {} ({owner}): {}\n{last}\n",
            task.id,
            task.status,
            span(state.now() - task.quiet_since),
            cut(&task.title),
        );
        if let Some(url) = state.public_url() {
            item.push_str(&format!("{url}/tasks/{}\n", task.id));
        }
        // The first task always fits: titles are cut, and the cap leaves room.
        if shown > 0 && text.chars().count() + item.chars().count() > DIGEST_CHARS {
            text.push_str(&format!(
                "\n… and {} more in the next message.",
                stuck.len() - shown
            ));
            break;
        }
        text.push_str(&item);
        shown += 1;
    }
    (text.trim_end().to_owned(), shown)
}

/// Reports the autopilot boards whose ready work sits unclaimed, in one message, each once
/// until it stalls anew. Only what was sent is recorded, so a failed send, or boards past
/// the message's length, go in the next check's message.
async fn send_stalled(state: &AppState, bot: &Bot, chat: i64, schedule: &Schedule) -> Result<()> {
    let quiet_ms =
        i64::try_from(schedule.stalled_after_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let stalled = state
        .write(move |cx| telegram_ops::stalled(cx, quiet_ms))
        .await?;
    if stalled.is_empty() {
        return Ok(());
    }
    let (text, shown) = stalled_text(state, &stalled, schedule);
    bot.send(json!({ "chat_id": chat, "text": text })).await?;
    let sent: Vec<_> = stalled
        .into_iter()
        .take(shown)
        .map(|b| (b.board, b.since))
        .collect();
    state
        .write(move |cx| telegram_ops::record_stalled(cx, &sent))
        .await
}

/// The stalled message, and how many of `stalled` it lists: per board, its ready count,
/// since when it stalled, and the coordinator that holds its lease.
fn stalled_text(
    state: &AppState,
    stalled: &[telegram_ops::Stalled],
    schedule: &Schedule,
) -> (String, usize) {
    let threshold =
        TimeDelta::seconds(i64::try_from(schedule.stalled_after_secs).unwrap_or(i64::MAX));
    let mut text = format!(
        "Stalled: {} autopilot {} with ready work and nothing claimed for {} or more.\n",
        stalled.len(),
        plural(stalled.len(), "board", "boards"),
        span(threshold),
    );
    let mut shown = 0;
    for board in stalled {
        let coordinator = match &board.coordinator {
            Some((handle, agent_state, seen)) => format!(
                "Coordinator {handle}: {agent_state}, last seen {}.",
                local(*seen, schedule.utc_offset)
            ),
            None => "No coordinator holds the board.".to_owned(),
        };
        let mut item = format!(
            "\n{}: {} ready auto {}, no task in doing, stalled for {} (since {}).\n{coordinator}\n",
            board.board,
            board.ready,
            plural(board.ready, "task", "tasks"),
            span(state.now() - board.since),
            local(board.since, schedule.utc_offset),
        );
        if let Some(url) = state.public_url() {
            item.push_str(&format!("{url}/boards/{}\n", board.board));
        }
        // The first board always fits: slugs and handles are short, and the cap leaves room.
        if shown > 0 && text.chars().count() + item.chars().count() > DIGEST_CHARS {
            text.push_str(&format!(
                "\n… and {} more in the next message.",
                stalled.len() - shown
            ));
            break;
        }
        text.push_str(&item);
        shown += 1;
    }
    (text.trim_end().to_owned(), shown)
}

/// Sends the digest of the local day once its time has passed, unless it was sent; nothing
/// is sent when nothing needs the user. The day is recorded only after the send, so a
/// digest that cannot be sent is tried again at the next check.
async fn send_digest(
    state: &AppState,
    bot: &Bot,
    chat: i64,
    at: NaiveTime,
    offset: FixedOffset,
) -> Result<()> {
    let Some(day) = digest_day(state.now().with_timezone(&offset), at) else {
        return Ok(());
    };
    let day = day.to_string();
    let sent = state.read(telegram_ops::digest_sent).await?;
    // Dates as `YYYY-MM-DD` sort as they compare.
    if sent.is_some_and(|sent| sent >= day) {
        return Ok(());
    }
    let digest = state.read(telegram_ops::digest).await?;
    if !digest.is_empty() {
        let text = digest_text(state, &digest, &day);
        bot.send(json!({ "chat_id": chat, "text": text })).await?;
    }
    state
        .write(move |cx| telegram_ops::record_digest(cx, &day))
        .await
}

/// The local day whose digest is due at `now`: today from `at` on, else yesterday for
/// `DIGEST_GRACE_MINS` after yesterday's `at`, so a check delayed past midnight (a slow
/// check, a restart) still sends a late digest.
fn digest_day(now: DateTime<FixedOffset>, at: NaiveTime) -> Option<NaiveDate> {
    let today = now.date_naive();
    if now.time() >= at {
        return Some(today);
    }
    let yesterday = today.pred_opt()?;
    let late = now.naive_local() - yesterday.and_time(at);
    (late <= TimeDelta::minutes(DIGEST_GRACE_MINS)).then_some(yesterday)
}

fn digest_text(state: &AppState, digest: &telegram_ops::Digest, day: &str) -> String {
    let (asks, tasks) = digest
        .boards
        .values()
        .fold((0, 0), |(a, t), (asks, tasks)| {
            (a + asks.len(), t + tasks.len())
        });
    let mut text = format!(
        "Needs you, {day}: {asks} open {}, {tasks} {} waiting on you.\n",
        plural(asks, "ask", "asks"),
        plural(tasks, "task", "tasks"),
    );
    // Board headings and items; only items count toward "… and N more".
    let mut lines = Vec::new();
    for (board, (asks, tasks)) in &digest.boards {
        lines.push((false, format!("\n{board}")));
        for ask in asks {
            let question = ask.question.lines().next().unwrap_or_default();
            lines.push((true, format!("· ask {}: {}", ask.id, cut(question))));
        }
        for task in tasks {
            let status = match task.status {
                board_proto::TaskStatus::Blocked => "blocked",
                _ => "waits on you",
            };
            lines.push((
                true,
                format!("· {} {status}: {}", task.id, cut(&task.title)),
            ));
        }
    }
    let mut left = asks + tasks;
    for (item, line) in lines {
        if text.chars().count() + line.chars().count() > DIGEST_CHARS {
            text.push_str(&format!("\n… and {left} more."));
            break;
        }
        text.push_str(&line);
        text.push('\n');
        left -= usize::from(item);
    }
    if digest.done_since_seen > 0 {
        text.push_str(&format!(
            "\nDone since you last looked: {} {}.",
            digest.done_since_seen,
            plural(digest.done_since_seen, "task", "tasks"),
        ));
    }
    if let Some(url) = state.public_url() {
        text.push_str(&format!("\n{url}/asks"));
    }
    text
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// `text` cut to `DIGEST_LINE_CHARS` characters.
fn cut(text: &str) -> String {
    if text.chars().count() <= DIGEST_LINE_CHARS {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(DIGEST_LINE_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// A time as the user reads it: local, to the minute.
fn local(at: Timestamp, offset: FixedOffset) -> String {
    at.with_timezone(&offset)
        .format("%Y-%m-%d %H:%M %:z")
        .to_string()
}

/// A duration in days and hours, or hours and minutes under a day.
fn span(d: TimeDelta) -> String {
    let minutes = d.num_minutes().max(0);
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days} d {hours} h")
    } else {
        format!("{hours} h {mins} min")
    }
}

async fn poll_loop(state: AppState, bot: Bot) {
    let mut offset: i64 = 0;
    loop {
        let body = json!({
            "offset": offset,
            "timeout": POLL_SECS,
            "allowed_updates": ["message", "callback_query"],
        });
        let updates = tokio::select! {
            () = state.stopped() => return,
            updates = bot.call::<Vec<Update>>("getUpdates", &body) => updates,
        };
        match updates {
            Ok(updates) => {
                for update in updates {
                    offset = offset.max(update.update_id + 1);
                    if let Err(error) = handle_update(&state, &bot, update).await {
                        tracing::error!(%error, "telegram update failed");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "telegram poll failed; retrying");
                tokio::select! {
                    () = state.stopped() => return,
                    () = tokio::time::sleep(RETRY) => {}
                }
            }
        }
    }
}

/// Parses callback data `ask:<id>:<option index>`.
fn parse_callback(data: &str) -> Option<(i64, usize)> {
    let mut parts = data.strip_prefix("ask:")?.split(':');
    let id = parts.next()?.parse().ok()?;
    let index = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((id, index))
}

/// The code in `/start <code>` (or `/start@<bot> <code>`); `None` for anything else.
fn start_code(text: &str) -> Option<&str> {
    let mut words = text.split_whitespace();
    let command = words.next()?;
    if command != "/start" && !command.starts_with("/start@") {
        return None;
    }
    let code = words.next()?;
    words.next().is_none().then_some(code)
}

/// Pairs the chat of a private `/start <code>` message that redeems a live code. Anything
/// else, a wrong or expired code included, gets no answer.
async fn try_pair(state: &AppState, bot: &Bot, message: &Message) -> Result<()> {
    if message.chat.kind.as_deref() != Some("private") {
        return Ok(());
    }
    let Some(code) = message.text.as_deref().and_then(start_code) else {
        return Ok(());
    };
    let (code, chat) = (code.to_owned(), message.chat.id);
    if !state
        .write(move |cx| telegram_ops::redeem(cx, &code, chat))
        .await?
    {
        return Ok(());
    }
    tracing::info!("telegram: paired a chat; asks go there from now on");
    bot.send(json!({
        "chat_id": chat,
        "text": "Paired: this chat now receives board asks and may answer them.",
    }))
    .await?;
    // Open asks, including those created while no chat was paired, go to the new chat.
    let open = state
        .read(|cx| asks::list(cx, Some(board_proto::AskStatus::Open), None))
        .await?;
    // One failed ask does not keep the others from the new chat.
    for ask in open {
        if let Err(error) = send_ask_to(state, bot, chat, ask.id).await {
            tracing::error!(%error, ask = ask.id, "telegram: sending an open ask failed");
        }
    }
    Ok(())
}

async fn handle_update(state: &AppState, bot: &Bot, update: Update) -> Result<()> {
    let chat = bot.chat(state).await?;
    if let Some(query) = update.callback_query {
        let from_chat = chat.is_some() && query.message.as_ref().map(|m| m.chat.id) == chat;
        let reply = if !from_chat {
            "Not accepted from this chat.".to_owned()
        } else if let Some((id, index)) = query.data.as_deref().and_then(parse_callback) {
            answer(state, id, Answer::Option(index)).await
        } else {
            "Unknown button.".to_owned()
        };
        let _: Value = bot
            .call(
                "answerCallbackQuery",
                &json!({ "callback_query_id": query.id, "text": reply }),
            )
            .await?;
        return Ok(());
    }
    let Some(message) = update.message else {
        return Ok(());
    };
    let Some(chat) = chat.filter(|c| *c == message.chat.id) else {
        // Another chat may pair (or, with a `--replace` code, re-pair) unless the chat is
        // configured.
        if bot.configured.is_none() {
            try_pair(state, bot, &message).await?;
        }
        return Ok(());
    };
    let (Some(text), Some(original)) = (message.text, message.reply_to_message) else {
        return Ok(());
    };
    let original_id = original.message_id;
    let Some(id) = state
        .read(move |cx| asks::by_telegram_message(cx, original_id))
        .await?
    else {
        return Ok(());
    };
    let reply = answer(state, id, Answer::Text(text)).await;
    bot.send(json!({
        "chat_id": chat,
        "text": reply,
        "reply_parameters": { "message_id": message.message_id },
    }))
    .await
    .map(|_| ())
}

enum Answer {
    Option(usize),
    Text(String),
}

/// Answers an ask from Telegram; returns the acknowledgement to show. When the answer
/// cannot be recorded (say the database is busy), the ask stays open and the
/// acknowledgement asks the user to answer again, since Telegram does not resend the
/// update.
async fn answer(state: &AppState, id: i64, answer: Answer) -> String {
    let result = state
        .write(move |cx| {
            let ask = get_ask(cx, id)?;
            let text = match answer {
                Answer::Option(index) => match ask.options.get(index) {
                    Some(option) => option.clone(),
                    None => {
                        return Err(crate::error::invalid(
                            "no such option",
                            "pick one of the buttons",
                        ));
                    }
                },
                Answer::Text(text) => text,
            };
            asks::validate_answer(
                &board_proto::AnswerAsk {
                    answer: text.clone(),
                },
                cx.s,
            )?;
            asks::answer(cx, id, &text, "telegram")?;
            Ok(text)
        })
        .await;
    match result {
        Ok(text) => format!("Answered ask {id}: {text}"),
        Err(Error::Api(e)) => format!("Ask {id}: {}", e.message),
        Err(error) => {
            tracing::error!(%error, ask = id, "telegram: recording an answer failed");
            format!(
                "Ask {id}: the board could not record this answer; it is still open, so answer again."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_callback, start_code};

    #[test]
    fn start_commands_carry_one_code() {
        assert_eq!(start_code("/start abc123"), Some("abc123"));
        assert_eq!(start_code("  /start@board_bot  abc123 "), Some("abc123"));
        assert_eq!(start_code("/start"), None);
        assert_eq!(start_code("/start a b"), None);
        assert_eq!(start_code("/starts abc"), None);
        assert_eq!(start_code("abc123"), None);
    }

    #[test]
    fn callback_data() {
        assert_eq!(parse_callback("ask:7:1"), Some((7, 1)));
        assert_eq!(parse_callback("ask:7"), None);
        assert_eq!(parse_callback("ask:7:1:2"), None);
        assert_eq!(parse_callback("task:7:1"), None);
    }
}
