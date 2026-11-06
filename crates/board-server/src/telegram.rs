//! Telegram notifier: sends each new ask to the configured chat, with its options as
//! buttons, and long-polls the Bot API for answers (a button press, or a reply to the ask's
//! message) from that chat only. No public endpoint is needed.
//!
//! The bot token is part of every Bot API URL, so request errors are logged without their
//! URL and the token never reaches the log.

use std::time::Duration;

use board_proto::{AgentRole, Event, EventType};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::config::TelegramConfig;
use crate::error::{Error, Result};
use crate::ops::asks;
use crate::state::AppState;
use crate::store::{find_task, get_agent, get_ask, get_board};

/// Long-poll timeout passed to `getUpdates`.
const POLL_SECS: u64 = 25;
const RETRY: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Bot {
    http: reqwest::Client,
    /// `<api_base>/bot<token>`: secret, never logged.
    base: String,
    chat_id: i64,
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
}

/// Starts the notifier when a token is configured (`BOARD_TELEGRAM_TOKEN` or
/// `bot_token`); `Ok(None)` otherwise. Runs until the state shuts down.
pub fn spawn_telegram(state: AppState, config: TelegramConfig) -> Result<Option<JoinHandle<()>>> {
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
        chat_id: config.chat_id,
    };
    let events = state.db().subscribe();
    let notify = tokio::spawn(notify_loop(state.clone(), bot.clone(), events));
    let poll = tokio::spawn(poll_loop(state, bot));
    Ok(Some(tokio::spawn(async move {
        let _ = tokio::join!(notify, poll);
    })))
}

async fn notify_loop(state: AppState, bot: Bot, mut events: broadcast::Receiver<Event>) {
    loop {
        let event = tokio::select! {
            () = state.stopped() => return,
            received = events.recv() => match received {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "telegram notifier fell behind the event feed");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };
        let result = match event.event_type {
            EventType::AskCreated => send_ask(&state, &bot, &event).await,
            EventType::AgentStateChanged => report_paused(&state, &bot, &event).await,
            _ => Ok(()),
        };
        if let Err(error) = result {
            tracing::error!(%error, seq = event.seq, "telegram notification failed");
        }
    }
}

async fn send_ask(state: &AppState, bot: &Bot, event: &Event) -> Result<()> {
    let Some(id) = event.payload.get("id").and_then(Value::as_i64) else {
        return Ok(());
    };
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
    if ask.options.is_empty() {
        text.push_str("Reply to this message to answer.");
    } else {
        text.push_str("Pick an option, or reply to this message to answer freely.");
    }
    if let Some(url) = state.public_url() {
        text.push_str(&format!("\n{url}/asks/{}", ask.id));
    }
    let mut body = json!({ "chat_id": bot.chat_id, "text": text });
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
    let message = bot.send(body).await?;
    state
        .write(move |cx| asks::set_telegram_message(cx, id, message.message_id))
        .await
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
    let until = agent
        .paused_until
        .map_or_else(|| "further notice".to_owned(), |t| t.to_rfc3339());
    let boards: Vec<String> = agent.boards.iter().map(ToString::to_string).collect();
    let text = format!(
        "Coordinator {} ({}) is paused until {until}.",
        agent.handle,
        boards.join(", ")
    );
    bot.send(json!({ "chat_id": bot.chat_id, "text": text }))
        .await
        .map(|_| ())
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

async fn handle_update(state: &AppState, bot: &Bot, update: Update) -> Result<()> {
    if let Some(query) = update.callback_query {
        let from_chat = query.message.as_ref().map(|m| m.chat.id) == Some(bot.chat_id);
        let reply = if !from_chat {
            "Not accepted from this chat.".to_owned()
        } else if let Some((id, index)) = query.data.as_deref().and_then(parse_callback) {
            answer(state, id, Answer::Option(index)).await?
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
    if message.chat.id != bot.chat_id {
        return Ok(());
    }
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
    let reply = answer(state, id, Answer::Text(text)).await?;
    bot.send(json!({
        "chat_id": bot.chat_id,
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

/// Answers an ask from Telegram; returns the acknowledgement to show.
async fn answer(state: &AppState, id: i64, answer: Answer) -> Result<String> {
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
        Ok(text) => Ok(format!("Answered ask {id}: {text}")),
        Err(Error::Api(e)) => Ok(format!("Ask {id}: {}", e.message)),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_callback;

    #[test]
    fn callback_data() {
        assert_eq!(parse_callback("ask:7:1"), Some((7, 1)));
        assert_eq!(parse_callback("ask:7"), None);
        assert_eq!(parse_callback("ask:7:1:2"), None);
        assert_eq!(parse_callback("task:7:1"), None);
    }
}
