use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use chrono::TimeDelta;

use super::T;
use crate::telegram::spawn_with;
use crate::{TelegramConfig, spawn_telegram};

const CHAT: i64 = 4242;
const TOKEN: &str = "123:test-token";
const A: &str = "t3:alpha";

/// A fake Bot API: records every call and hands out queued updates.
#[derive(Clone, Default)]
struct Fake {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
    /// How long to hold the answer to the "Paired" message, which keeps the notifier
    /// between pairing a chat and sending it the open asks.
    paired_delay: Arc<Mutex<Duration>>,
    /// How many sendMessage calls to refuse next, as Telegram does when rate limited;
    /// refused calls are not recorded.
    fail_sends: Arc<Mutex<usize>>,
}

impl Fake {
    fn calls(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn push(&self, update: Value) {
        self.updates.lock().unwrap().push_back(update);
    }

    /// Waits until `method` has been called `count` times.
    async fn wait(&self, method: &str, count: usize) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let calls = self.calls(method);
                if calls.len() >= count {
                    return calls;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{method} called fewer than {count} times"))
    }
}

async fn bot_api(
    State(fake): State<Fake>,
    Path((bot, method)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(bot, format!("bot{TOKEN}"));
    let paired = method == "sendMessage"
        && body["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("Paired"));
    if method == "sendMessage" {
        let mut fail = fake.fail_sends.lock().unwrap();
        if *fail > 0 {
            *fail -= 1;
            return Json(json!({ "ok": false, "description": "Too Many Requests: retry after 1" }));
        }
    }
    fake.calls.lock().unwrap().push((method.clone(), body));
    if paired {
        let delay = *fake.paired_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
    }
    let result = match method.as_str() {
        "getUpdates" => {
            let next = fake.updates.lock().unwrap().pop_front();
            match next {
                Some(update) => json!([update]),
                None => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    json!([])
                }
            }
        }
        "sendMessage" => {
            let sent = fake.calls("sendMessage").len();
            json!({ "message_id": 1000 + sent, "chat": { "id": CHAT } })
        }
        _ => json!(true),
    };
    Json(json!({ "ok": true, "result": result }))
}

async fn wait_for_answer(t: &T, id: i64) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let asks = t.ok("GET", "/v1/asks?status=answered", None, None).await;
            if let Some(ask) = asks.as_array().unwrap().iter().find(|a| a["id"] == id) {
                return ask.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("ask {id} was not answered"))
}

/// Serves a fake Bot API; returns it and its base URL.
async fn fake_api() -> (Fake, String) {
    let fake = Fake::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    let server = Router::new()
        .route("/{bot}/{method}", post(bot_api))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    (fake, api_base)
}

#[tokio::test]
async fn asks_round_trip_through_telegram() {
    let (fake, api_base) = fake_api().await;

    let t = T::with(|c| c.public_url = Some("https://board.example".to_owned()));
    let mut config = TelegramConfig::new(CHAT);
    config.bot_token = Some(TOKEN.to_owned());
    config.api_base = api_base;
    let notifier = spawn_telegram(t.state.clone(), config)
        .unwrap()
        .expect("a token is set");

    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let task = t.task(A, "factory", "Publish the crate").await;
    let first = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "task": task, "kind": "decision",
                "question": "Publish now?", "options": ["yes", "later"],
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let sent = fake.wait("sendMessage", 1).await;
    assert_eq!(sent[0]["chat_id"], CHAT);
    let text = sent[0]["text"].as_str().unwrap();
    assert!(text.contains("Publish now?"), "{text}");
    assert!(text.contains("Task factory-1: Publish the crate"), "{text}");
    assert!(
        text.contains(&format!("https://board.example/asks/{first}")),
        "{text}"
    );
    assert_eq!(
        sent[0]["reply_markup"]["inline_keyboard"][1][0],
        json!({ "text": "later", "callback_data": format!("ask:{first}:1") })
    );

    // A button press from another chat is refused.
    fake.push(json!({
        "update_id": 1,
        "callback_query": {
            "id": "cb-1",
            "data": format!("ask:{first}:0"),
            "message": { "message_id": 1001, "chat": { "id": 666 } },
        },
    }));
    let acks = fake.wait("answerCallbackQuery", 1).await;
    assert_eq!(
        acks[0],
        json!({ "callback_query_id": "cb-1", "text": "Not accepted from this chat." })
    );
    assert_eq!(
        t.ok("GET", "/v1/asks?status=open", None, None)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A button press from the chat answers with the option.
    fake.push(json!({
        "update_id": 2,
        "callback_query": {
            "id": "cb-2",
            "data": format!("ask:{first}:1"),
            "message": { "message_id": 1001, "chat": { "id": CHAT } },
        },
    }));
    let ask = wait_for_answer(&t, first).await;
    assert_eq!(
        (ask["answer"].as_str(), ask["answered_via"].as_str()),
        (Some("later"), Some("telegram"))
    );
    let acks = fake.wait("answerCallbackQuery", 2).await;
    assert_eq!(acks[1]["text"], format!("Answered ask {first}: later"));

    // A reply to an ask's message answers with the reply's text.
    let second = t
        .ok("POST", "/v1/asks", Some(A), Some(json!({
            "agent": A, "board": "factory", "kind": "action", "question": "Log in to the registry",
        })))
        .await["id"]
        .as_i64()
        .unwrap();
    let sent = fake.wait("sendMessage", 2).await;
    assert!(sent[1].get("reply_markup").is_none());
    let message_id = 1002;
    fake.push(json!({
        "update_id": 3,
        "message": {
            "message_id": 77,
            "chat": { "id": CHAT },
            "text": "done, logged in",
            "reply_to_message": { "message_id": message_id, "chat": { "id": CHAT } },
        },
    }));
    let ask = wait_for_answer(&t, second).await;
    assert_eq!(ask["answer"], "done, logged in");
    let sent = fake.wait("sendMessage", 3).await;
    assert_eq!(sent[2]["reply_parameters"]["message_id"], 77);
    assert_eq!(
        sent[2]["text"],
        format!("Answered ask {second}: done, logged in")
    );

    // Offsets advance past handled updates.
    let mut polls = fake.calls("getUpdates");
    while polls.last().unwrap()["offset"] != 4 {
        polls = fake.wait("getUpdates", polls.len() + 1).await;
    }
    assert!(polls.iter().all(|p| p["offset"].as_i64().unwrap() <= 4));
    assert_eq!(
        polls[0]["allowed_updates"],
        json!(["message", "callback_query"])
    );

    t.state.shutdown();
    tokio::time::timeout(Duration::from_secs(10), notifier)
        .await
        .expect("the notifier stops on shutdown")
        .unwrap();
}

#[tokio::test]
async fn a_lagging_notifier_replays_missed_asks_once() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let since = t.ok("GET", "/healthz", None, None).await["seq"]
        .as_i64()
        .unwrap();
    let mut asks = Vec::new();
    for n in 0..3 {
        let ask = t
            .ok("POST", "/v1/asks", Some(A), Some(json!({
                "agent": A, "board": "factory", "kind": "action", "question": format!("Step {n}"),
            })))
            .await;
        asks.push(ask["id"].as_i64().unwrap());
    }
    let page = t
        .ok("GET", &format!("/v1/events?since={since}"), None, None)
        .await;
    let events: Vec<board_proto::Event> = serde_json::from_value(page["events"].clone()).unwrap();
    assert_eq!(events.len(), 3);

    // A one-slot feed that already overflowed: the notifier sees a lag first, then the
    // last event again.
    let (feed, events_rx) = tokio::sync::broadcast::channel(1);
    for event in events {
        feed.send(event).unwrap();
    }
    let mut config = TelegramConfig::new(CHAT);
    config.bot_token = Some(TOKEN.to_owned());
    config.api_base = api_base;
    let notifier = spawn_with(
        t.state.clone(),
        config,
        events_rx,
        since,
        Duration::from_secs(60),
        None,
    )
    .unwrap()
    .expect("a token is set");

    let sent = fake.wait("sendMessage", 3).await;
    for (message, n) in sent.iter().zip(0..) {
        let text = message["text"].as_str().unwrap();
        assert!(text.contains(&format!("Step {n}")), "{text}");
    }
    // Later live events still arrive, and nothing is sent twice.
    let before = t.ok("GET", "/healthz", None, None).await["seq"]
        .as_i64()
        .unwrap();
    t.ok(
        "POST",
        "/v1/asks",
        Some(A),
        Some(json!({
            "agent": A, "board": "factory", "kind": "action", "question": "Step 3",
        })),
    )
    .await;
    let page = t
        .ok("GET", &format!("/v1/events?since={before}"), None, None)
        .await;
    let live: Vec<board_proto::Event> = serde_json::from_value(page["events"].clone()).unwrap();
    feed.send(live.last().unwrap().clone()).unwrap();
    let sent = fake.wait("sendMessage", 4).await;
    assert!(sent[3]["text"].as_str().unwrap().contains("Step 3"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.calls("sendMessage").len(), 4);

    t.state.shutdown();
    tokio::time::timeout(Duration::from_secs(10), notifier)
        .await
        .expect("the notifier stops on shutdown")
        .unwrap();
}

impl Fake {
    /// Queues `update` and waits until the notifier has handled it: it polls again only
    /// after handling what the last poll returned.
    async fn handle(&self, update: Value) {
        let polls = self.calls("getUpdates").len();
        self.push(update);
        self.wait("getUpdates", polls + 2).await;
    }
}

fn start(update_id: i64, chat: i64, kind: &str, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": 500 + update_id,
            "chat": { "id": chat, "type": kind },
            "text": text,
        },
    })
}

/// A notifier with a token and no configured chat.
fn unpaired(api_base: String) -> TelegramConfig {
    TelegramConfig {
        bot_token: Some(TOKEN.to_owned()),
        api_base,
        ..TelegramConfig::default()
    }
}

async fn ask(t: &T, question: &str) -> i64 {
    t.ok(
        "POST",
        "/v1/asks",
        Some(A),
        Some(json!({
            "agent": A, "board": "factory", "kind": "action", "question": question,
        })),
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn a_one_time_code_pairs_the_first_private_chat() {
    const PHONE: i64 = 555;
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let notifier = spawn_telegram(t.state.clone(), unpaired(api_base))
        .unwrap()
        .expect("a token alone starts the notifier");

    // Unpaired: asks are not sent, and no code is accepted before one exists.
    let before = ask(&t, "Before pairing").await;
    fake.handle(start(1, PHONE, "private", "/start nothing"))
        .await;
    let code = t.state.create_telegram_pairing(false).unwrap();
    // Wrong codes, groups and other commands get no answer.
    fake.handle(start(2, PHONE, "private", "/start wrongcode123"))
        .await;
    fake.handle(start(3, -42, "group", &format!("/start {code}")))
        .await;
    fake.handle(start(4, PHONE, "private", &code)).await;
    assert!(fake.calls("sendMessage").is_empty());

    // The code pairs the chat and is used up; the ask created while unpaired follows.
    fake.handle(start(
        5,
        PHONE,
        "private",
        &format!("/start {}", code.to_uppercase()),
    ))
    .await;
    let sent = fake.wait("sendMessage", 2).await;
    assert_eq!(sent[0]["chat_id"], PHONE);
    assert!(sent[0]["text"].as_str().unwrap().starts_with("Paired"));
    assert_eq!(sent[1]["chat_id"], PHONE);
    assert!(sent[1]["text"].as_str().unwrap().contains("Before pairing"));

    // From then on only that chat receives asks and may answer.
    let id = ask(&t, "After pairing").await;
    let sent = fake.wait("sendMessage", 3).await;
    assert_eq!(sent[2]["chat_id"], PHONE);
    assert!(sent[2]["text"].as_str().unwrap().contains("After pairing"));
    fake.handle(start(6, 777, "private", &format!("/start {code}")))
        .await;
    fake.handle(json!({
        "update_id": 7,
        "message": {
            "message_id": 90, "chat": { "id": 777, "type": "private" }, "text": "done",
            "reply_to_message": { "message_id": 1003, "chat": { "id": PHONE } },
        },
    }))
    .await;
    assert_eq!(fake.calls("sendMessage").len(), 3);
    fake.push(json!({
        "update_id": 8,
        "message": {
            "message_id": 91, "chat": { "id": PHONE, "type": "private" }, "text": "done",
            "reply_to_message": { "message_id": 1003, "chat": { "id": PHONE } },
        },
    }));
    assert_eq!(wait_for_answer(&t, id).await["answer"], "done");
    let sent = fake.wait("sendMessage", 4).await;
    assert!(sent[3]["text"].as_str().unwrap().starts_with("Answered"));

    // A second code needs --replace while a chat is paired, and the paired chat keeps
    // receiving asks until another chat redeems it.
    assert!(t.state.create_telegram_pairing(false).is_err());
    let code = t.state.create_telegram_pairing(true).unwrap();
    let still = ask(&t, "Still paired").await;
    let sent = fake.wait("sendMessage", 5).await;
    assert_eq!(sent[4]["chat_id"], PHONE);
    assert!(sent[4]["text"].as_str().unwrap().contains("Still paired"));

    // Redeeming it moves the chat: the open asks are sent again, and replies to the old
    // chat's messages no longer answer anything.
    fake.handle(start(9, 777, "private", &format!("/start {code}")))
        .await;
    let sent = fake.wait("sendMessage", 8).await;
    assert!(sent[5..].iter().all(|m| m["chat_id"] == 777));
    assert!(sent[5]["text"].as_str().unwrap().starts_with("Paired"));
    let resent: Vec<&str> = sent[6..]
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert!(resent.iter().any(|t| t.contains("Before pairing")));
    assert!(resent.iter().any(|t| t.contains("Still paired")));
    fake.handle(json!({
        "update_id": 10,
        "message": {
            "message_id": 92, "chat": { "id": 777, "type": "private" }, "text": "stale",
            "reply_to_message": { "message_id": 1002, "chat": { "id": PHONE } },
        },
    }))
    .await;
    fake.handle(json!({
        "update_id": 11,
        "message": {
            "message_id": 93, "chat": { "id": PHONE, "type": "private" }, "text": "old chat",
            "reply_to_message": { "message_id": 1005, "chat": { "id": PHONE } },
        },
    }))
    .await;
    assert_eq!(
        fake.calls("sendMessage").len(),
        8,
        "neither reply is answered"
    );
    let open = t.ok("GET", "/v1/asks?status=open", None, None).await;
    let open: Vec<i64> = open
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_i64().unwrap())
        .collect();
    assert!(open.contains(&before) && open.contains(&still));

    t.state.shutdown();
    tokio::time::timeout(Duration::from_secs(10), notifier)
        .await
        .expect("the notifier stops on shutdown")
        .unwrap();
}

#[tokio::test]
async fn pairing_codes_expire_and_a_configured_chat_wins() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let notifier = spawn_telegram(t.state.clone(), unpaired(api_base.clone()))
        .unwrap()
        .unwrap();
    let code = t.state.create_telegram_pairing(false).unwrap();
    t.clock.advance(chrono::TimeDelta::minutes(15));
    fake.handle(start(1, 555, "private", &format!("/start {code}")))
        .await;
    assert!(
        fake.calls("sendMessage").is_empty(),
        "an expired code pairs nothing"
    );
    t.state.shutdown();
    notifier.await.unwrap();

    // With `[telegram] chat_id`, a live code changes nothing.
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let mut config = unpaired(api_base);
    config.chat_id = Some(CHAT);
    let notifier = spawn_telegram(t.state.clone(), config).unwrap().unwrap();
    let code = t.state.create_telegram_pairing(false).unwrap();
    fake.handle(start(1, 555, "private", &format!("/start {code}")))
        .await;
    ask(&t, "Configured").await;
    let sent = fake.wait("sendMessage", 1).await;
    assert_eq!(sent[0]["chat_id"], CHAT);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.calls("sendMessage").len(), 1);
    t.state.shutdown();
    notifier.await.unwrap();
}

#[tokio::test]
async fn a_withdrawn_ask_is_marked_in_telegram() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    let mut config = TelegramConfig::new(CHAT);
    config.bot_token = Some(TOKEN.to_owned());
    config.api_base = api_base;
    let notifier = spawn_telegram(t.state.clone(), config)
        .unwrap()
        .expect("a token is set");
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let id = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "kind": "decision",
                "question": "Publish now?", "options": ["yes", "later"],
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    fake.wait("sendMessage", 1).await;
    t.ok(
        "POST",
        &format!("/v1/asks/{id}/withdraw"),
        Some(A),
        Some(json!({ "agent": A })),
    )
    .await;
    let edits = fake.wait("editMessageText", 1).await;
    assert_eq!(
        (
            edits[0]["chat_id"].as_i64(),
            edits[0]["message_id"].as_i64()
        ),
        (Some(CHAT), Some(1001))
    );
    let text = edits[0]["text"].as_str().unwrap();
    assert!(text.contains("Publish now?"), "{text}");
    assert!(
        text.ends_with("Withdrawn by the agent; no answer is needed."),
        "{text}"
    );
    assert!(!text.contains("answer freely"), "{text}");
    assert!(edits[0].get("reply_markup").is_none(), "the buttons go");

    t.state.shutdown();
    tokio::time::timeout(Duration::from_secs(10), notifier)
        .await
        .expect("the notifier stops on shutdown")
        .unwrap();
}

#[tokio::test]
async fn an_answer_that_fails_to_record_is_reported_and_stays_answerable() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    let mut config = TelegramConfig::new(CHAT);
    config.bot_token = Some(TOKEN.to_owned());
    config.api_base = api_base;
    let notifier = spawn_telegram(t.state.clone(), config).unwrap().unwrap();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let id = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "kind": "decision",
                "question": "Publish now?", "options": ["yes", "later"],
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    fake.wait("sendMessage", 1).await;

    // The database refuses to record answers for a while.
    let sql = |sql: &'static str| {
        t.state.write(move |cx| {
            cx.db.execute_batch(sql)?;
            Ok(())
        })
    };
    sql(
        "CREATE TRIGGER refuse_answers BEFORE UPDATE OF status ON asks \
         BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
    )
    .await
    .unwrap();
    let press = |update_id: i64| {
        json!({
            "update_id": update_id,
            "callback_query": {
                "id": format!("cb-{update_id}"),
                "data": format!("ask:{id}:0"),
                "message": { "message_id": 1001, "chat": { "id": CHAT } },
            },
        })
    };
    fake.push(press(1));
    let acks = fake.wait("answerCallbackQuery", 1).await;
    assert_eq!(
        acks[0]["text"],
        format!(
            "Ask {id}: the board could not record this answer; it is still open, so answer again."
        )
    );
    let open = t.ok("GET", "/v1/asks?status=open", None, None).await;
    assert_eq!(open.as_array().unwrap().len(), 1, "{open}");

    // Pressing again once the database is back answers it.
    sql("DROP TRIGGER refuse_answers;").await.unwrap();
    fake.push(press(2));
    assert_eq!(wait_for_answer(&t, id).await["answer"], "yes");
    let acks = fake.wait("answerCallbackQuery", 2).await;
    assert_eq!(acks[1]["text"], format!("Answered ask {id}: yes"));

    t.state.shutdown();
    notifier.await.unwrap();
}

#[tokio::test]
async fn an_ask_created_while_a_chat_pairs_is_sent_once() {
    const PHONE: i64 = 555;
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let notifier = spawn_telegram(t.state.clone(), unpaired(api_base))
        .unwrap()
        .unwrap();
    *fake.paired_delay.lock().unwrap() = Duration::from_millis(300);
    let code = t.state.create_telegram_pairing(false).unwrap();
    fake.push(start(1, PHONE, "private", &format!("/start {code}")));
    // The chat is paired once the notifier says so; the open asks follow after it.
    fake.wait("sendMessage", 1).await;
    ask(&t, "Mid-pairing").await;
    let sent = fake.wait("sendMessage", 2).await;
    assert!(sent[1]["text"].as_str().unwrap().contains("Mid-pairing"));
    fake.wait("getUpdates", fake.calls("getUpdates").len() + 2)
        .await;
    assert_eq!(fake.calls("sendMessage").len(), 2, "the ask is sent once");

    t.state.shutdown();
    notifier.await.unwrap();
}

#[tokio::test]
async fn a_send_claim_holds_only_for_the_current_chat() {
    use crate::ops::asks::{claim_telegram_send, set_telegram_message, telegram_message};
    use crate::ops::telegram::redeem;

    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let id = ask(&t, "Claimed").await;
    let other = ask(&t, "Configured").await;
    let pair = |chat: i64, replace: bool| {
        let code = t.state.create_telegram_pairing(replace).unwrap();
        t.state.write(move |cx| redeem(cx, &code, chat))
    };
    let claim = |id: i64, chat: i64, paired: bool| {
        t.state
            .write(move |cx| claim_telegram_send(cx, id, chat, paired))
    };
    assert!(pair(1, false).await.unwrap());
    assert!(claim(id, 1, true).await.unwrap());
    assert!(!claim(id, 1, true).await.unwrap(), "claimed once");

    // Pairing another chat while chat 1's send is in flight drops its claim: the message
    // is not recorded, the old chat cannot claim again, and the new one can.
    assert!(pair(2, true).await.unwrap());
    let status = t
        .state
        .write(move |cx| set_telegram_message(cx, id, 1, 500))
        .await
        .unwrap();
    assert_eq!(status, board_proto::AskStatus::Open);
    let sent = |id: i64| t.state.read(move |cx| telegram_message(cx, id));
    assert_eq!(sent(id).await.unwrap(), None);
    assert!(!claim(id, 1, true).await.unwrap(), "not the paired chat");
    assert!(claim(id, 2, true).await.unwrap());
    t.state
        .write(move |cx| set_telegram_message(cx, id, 2, 501))
        .await
        .unwrap();
    assert_eq!(sent(id).await.unwrap(), Some((2, 501)));

    // A configured chat is not checked against the paired one.
    assert!(claim(other, 9, false).await.unwrap());
}

/// A notifier for a configured chat that checks the scheduled alerts every 20 ms, with the
/// digest off unless `configure` sets it. Events before it starts are not notified.
struct Scheduled {
    handle: tokio::task::JoinHandle<()>,
    checks: tokio::sync::watch::Receiver<u64>,
}

impl Scheduled {
    fn start(t: &T, api_base: String, configure: impl FnOnce(&mut TelegramConfig)) -> Self {
        let mut config = TelegramConfig::new(CHAT);
        config.bot_token = Some(TOKEN.to_owned());
        config.api_base = api_base;
        config.digest_at = None;
        configure(&mut config);
        let events = t.state.db().subscribe();
        let since = t
            .state
            .db()
            .read(t.state.now(), t.state.settings(), |cx| cx.max_seq())
            .unwrap();
        let (checked, checks) = tokio::sync::watch::channel(0);
        let handle = spawn_with(
            t.state.clone(),
            config,
            events,
            since,
            Duration::from_millis(20),
            Some(checked),
        )
        .unwrap()
        .expect("a token is set");
        Self { handle, checks }
    }

    /// The texts sent so far, once a whole check has run after this call, so it saw the
    /// clock as the test left it.
    async fn sent_after_check(&mut self, fake: &Fake) -> Vec<String> {
        let start = *self.checks.borrow_and_update();
        tokio::time::timeout(Duration::from_secs(10), async {
            while *self.checks.borrow_and_update() < start + 2 {
                self.checks.changed().await.unwrap();
            }
        })
        .await
        .expect("the notifier finished no check");
        texts(fake)
    }
}

impl Drop for Scheduled {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn texts(fake: &Fake) -> Vec<String> {
    fake.calls("sendMessage")
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_owned())
        .collect()
}

fn stuck_after() -> TimeDelta {
    TimeDelta::seconds(i64::try_from(TelegramConfig::default().stuck_after_secs).unwrap())
}

#[tokio::test]
async fn a_quiet_task_is_reported_stuck_once_until_it_changes() {
    let (fake, api_base) = fake_api().await;
    let t = T::with(|c| c.public_url = Some("https://board.example".to_owned()));
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let id = t.task(A, "factory", "Ship the parser").await;
    assert!(t.claim(A, &id).await.0.is_success());
    let mut notifier = Scheduled::start(&t, api_base, |_| {});

    // Just under the threshold: nothing.
    t.advance(stuck_after() - TimeDelta::minutes(1));
    assert!(notifier.sent_after_check(&fake).await.is_empty());

    t.advance(TimeDelta::minutes(2));
    let sent = fake.wait("sendMessage", 1).await;
    assert_eq!(sent[0]["chat_id"], CHAT);
    let text = sent[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "Stuck: 1 task with no update for 4 h 0 min or more.\n\n\
             {id} doing, quiet for 4 h 1 min (owner t3:"
        )),
        "{text}"
    );
    assert!(
        text.contains("): Ship the parser\nLast: task_claimed by "),
        "{text}"
    );
    assert!(
        text.ends_with(&format!("https://board.example/tasks/{id}")),
        "{text}"
    );
    // Still quiet: no repeat.
    t.advance(stuck_after());
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);

    // A note is a change: the task is reported again once it goes quiet again.
    t.ok(
        "POST",
        &format!("/v1/tasks/{id}/notes"),
        Some(A),
        Some(json!({ "agent": A, "kind": "note", "text": "note: still parsing" })),
    )
    .await;
    t.advance(stuck_after() - TimeDelta::minutes(1));
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
    t.advance(TimeDelta::minutes(2));
    let sent = fake.wait("sendMessage", 2).await;
    let text = sent[1]["text"].as_str().unwrap();
    assert!(text.contains("Last: note_added by "), "{text}");

    // Finished: never again.
    assert!(t.done(A, &id, super::check(0)).await.0.is_success());
    t.advance(stuck_after() * 3);
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 2);
}

#[tokio::test]
async fn quiet_tasks_share_one_message_and_a_failed_send_is_retried() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    t.board("old", &[]).await;
    let doing = t.task(A, "factory", "Ship the parser").await;
    let review = t.task(A, "factory", "Review the parser").await;
    let closed = t.task(A, "old", "On a closed board").await;
    let open = t.task(A, "factory", "Not started").await;
    t.exec(&format!(
        "UPDATE tasks SET status = 'doing', owner = '{A}' WHERE id IN ('{doing}', '{closed}');
         UPDATE tasks SET status = 'review', owner = '{A}' WHERE id = '{review}';
         UPDATE boards SET status = 'closed' WHERE slug = 'old';"
    ));
    *fake.fail_sends.lock().unwrap() = 1;
    let mut notifier = Scheduled::start(&t, api_base, |_| {});

    t.advance(stuck_after() + TimeDelta::minutes(1));
    // The first send is refused; nothing was recorded, so the next check sends it.
    let sent = fake.wait("sendMessage", 1).await;
    assert_eq!(*fake.fail_sends.lock().unwrap(), 0);
    let text = sent[0]["text"].as_str().unwrap();
    assert!(text.starts_with("Stuck: 2 tasks with no update"), "{text}");
    assert!(
        text.contains(&format!("\n{doing} doing, quiet for ")),
        "{text}"
    );
    assert!(
        text.contains(&format!("\n{review} review, quiet for ")),
        "{text}"
    );
    for id in [&closed, &open] {
        assert!(!text.contains(id.as_str()), "{id}: {text}");
    }
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
}

#[tokio::test]
async fn a_long_stuck_list_continues_in_the_next_message() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    for _ in 0..24 {
        t.task(A, "factory", "Long").await;
    }
    let long = "x".repeat(130);
    t.exec(&format!(
        "UPDATE tasks SET status = 'doing', title = '{long}' WHERE board = 'factory'"
    ));
    let mut notifier = Scheduled::start(&t, api_base, |_| {});

    t.advance(stuck_after() + TimeDelta::minutes(1));
    let sent = fake.wait("sendMessage", 2).await;
    let listed = |text: &str| {
        text.lines()
            .filter(|l| l.starts_with("factory-"))
            .map(|l| l.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let first = sent[0]["text"].as_str().unwrap();
    assert!(first.starts_with("Stuck: 24 tasks "), "{first}");
    assert!(first.chars().count() <= 4096, "{first}");
    let shown = listed(first);
    assert!(!shown.is_empty() && shown.len() < 24, "{first}");
    assert!(
        first.ends_with(&format!(
            "… and {} more in the next message.",
            24 - shown.len()
        )),
        "{first}"
    );
    // The rest follow at the next check; each task is listed once.
    let second = sent[1]["text"].as_str().unwrap();
    assert!(
        second.starts_with(&format!("Stuck: {} tasks ", 24 - shown.len())),
        "{second}"
    );
    let mut all: Vec<_> = shown.into_iter().chain(listed(second)).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 24);
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 2);
}

/// Marks `agent` stopped by a usage limit, as its hook does, until `until` (or until
/// further notice).
async fn pause(t: &T, agent: &str, until: Option<chrono::DateTime<chrono::Utc>>) {
    t.ok(
        "POST",
        &format!("/v1/agents/{agent}/state"),
        Some(agent),
        Some(json!({ "state": "paused", "paused_until": until })),
    )
    .await;
}

/// Marks `agent` ended, as the hook does when T3 closes its idle session.
async fn end(t: &T, agent: &str) {
    t.ok(
        "POST",
        &format!("/v1/agents/{agent}/state"),
        Some(agent),
        Some(json!({ "state": "ended" })),
    )
    .await;
}

/// The stuck alerts among `texts`.
fn stuck_only(texts: Vec<String>) -> Vec<String> {
    texts
        .into_iter()
        .filter(|text| text.starts_with("Stuck:"))
        .collect()
}

#[tokio::test]
async fn finished_and_paused_tasks_are_not_reported() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let done = t.task(A, "factory", "Finish quickly").await;
    assert!(t.claim(A, &done).await.0.is_success());
    let paused = t.task(A, "factory", "Wait for the reset").await;
    let mut notifier = Scheduled::start(&t, api_base, |_| {});

    // Done before the threshold, and the done event itself: no message.
    assert!(t.done(A, &done, super::check(0)).await.0.is_success());
    assert!(t.claim(A, &paused).await.0.is_success());
    // The owner stops on a usage limit, which has its own message; T3 closes its idle
    // session meanwhile.
    let reset = t.state.now() + stuck_after() * 3;
    pause(&t, A, Some(reset)).await;
    t.advance(TimeDelta::minutes(30));
    end(&t, A).await;
    t.advance(stuck_after() * 2);
    assert_eq!(
        stuck_only(notifier.sent_after_check(&fake).await),
        Vec::<String>::new()
    );
    // Reset less than a quiet period ago: T3 has just resumed it.
    t.advance(stuck_after() * 3 / 2);
    assert_eq!(
        stuck_only(notifier.sent_after_check(&fake).await),
        Vec::<String>::new()
    );
    // Quiet for a whole period after the reset: reported; the done task never is.
    t.advance(stuck_after());
    let texts = stuck_only(notifier.sent_after_check(&fake).await);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains(&format!("\n{paused} doing")), "{texts:?}");
    assert!(!texts[0].contains(done.as_str()), "{texts:?}");
    assert_eq!(stuck_only(notifier.sent_after_check(&fake).await), texts);
}

#[tokio::test]
async fn an_owner_resumed_before_the_reset_restarts_the_quiet_time_then() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let task = t.task(A, "factory", "Wait for the reset").await;
    assert!(t.claim(A, &task).await.0.is_success());
    let mut notifier = Scheduled::start(&t, api_base, |_| {});

    // A weekly limit, days away; the user resumes the owner on another account at once.
    pause(&t, A, Some(t.state.now() + TimeDelta::days(3))).await;
    t.advance(TimeDelta::minutes(1));
    t.ok(
        "POST",
        &format!("/v1/agents/{A}/state"),
        Some(A),
        Some(json!({ "state": "running" })),
    )
    .await;
    t.advance(stuck_after() / 2);
    assert_eq!(
        stuck_only(notifier.sent_after_check(&fake).await),
        Vec::<String>::new()
    );
    // A whole quiet period after the resume, not the reset: reported.
    t.advance(stuck_after());
    let texts = stuck_only(notifier.sent_after_check(&fake).await);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].contains(&format!("\n{task} doing")), "{texts:?}");
}

#[tokio::test]
async fn a_coordinator_paused_until_further_notice_holds_back_its_tasks() {
    const W: &str = "t3:worker";
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.register(W, None, None).await;
    t.board("factory", &[]).await;
    let coordinator = t.task(A, "factory", "Coordinate").await;
    assert!(t.claim(A, &coordinator).await.0.is_success());
    let worker = t.task(A, "factory", "Build it").await;
    assert!(t.claim(W, &worker).await.0.is_success());
    // With no reset time only the user resumes them: a coordinator's pause is reported and
    // holds its task back; a worker's is not reported, so its task is.
    t.exec(&format!(
        "UPDATE agents SET role = 'coordinator' WHERE id = '{A}'"
    ));
    pause(&t, A, None).await;
    pause(&t, W, None).await;
    let mut notifier = Scheduled::start(&t, api_base, |_| {});

    t.advance(stuck_after() * 2);
    let texts = stuck_only(notifier.sent_after_check(&fake).await);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].starts_with("Stuck: 1 task "), "{texts:?}");
    assert!(texts[0].contains(&format!("\n{worker} doing")), "{texts:?}");
    assert_eq!(stuck_only(notifier.sent_after_check(&fake).await), texts);
}

#[tokio::test]
async fn a_usage_limit_is_reported_once_per_reset() {
    const W: &str = "t3:worker";
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.register(W, None, None).await;
    t.board("factory", &[A]).await;
    t.exec(&format!(
        "UPDATE agents SET role = 'coordinator' WHERE id = '{A}'"
    ));
    let tokyo = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
    let _notifier = Scheduled::start(&t, api_base, |c| {
        c.stuck_after_secs = 0;
        c.utc_offset = tokyo;
    });
    let reset = t.state.now() + TimeDelta::hours(9);

    // A report that fails to send is made by the next pause on the limit.
    *fake.fail_sends.lock().unwrap() = 1;
    pause(&t, A, Some(reset)).await;
    pause(&t, A, Some(reset)).await;
    let sent = fake.wait("sendMessage", 1).await;
    assert_eq!(*fake.fail_sends.lock().unwrap(), 0);
    let first = sent[0]["text"].as_str().unwrap();
    let local = reset
        .with_timezone(&tokyo)
        .format("%Y-%m-%d %H:%M +09:00")
        .to_string();
    assert!(first.starts_with("Usage limit: Coordinator "), "{first}");
    assert!(
        first.contains(&format!(" is paused until {local}. Other agents")),
        "{first}"
    );

    // While the limit holds: its session ends, a wake resumes it and it stops again, and a
    // worker stops on the same limit. None of it is reported again.
    for _ in 0..3 {
        t.advance(TimeDelta::minutes(30));
        end(&t, A).await;
        t.register(A, None, None).await;
        pause(&t, A, Some(reset)).await;
    }
    pause(&t, W, Some(reset)).await;
    // Another limit, with its own reset, is reported, for a worker too.
    pause(&t, W, Some(reset + TimeDelta::hours(1))).await;
    let sent = fake.wait("sendMessage", 2).await;
    let second = sent[1]["text"].as_str().unwrap();
    assert!(second.starts_with("Usage limit: Agent "), "{second}");
    // A coordinator paused until further notice is reported each time; a worker is not.
    pause(&t, W, None).await;
    // (Registering again made it a worker: no board names it coordinator.)
    t.exec(&format!(
        "UPDATE agents SET role = 'coordinator' WHERE id = '{A}'"
    ));
    pause(&t, A, None).await;
    pause(&t, A, None).await;
    let sent = fake.wait("sendMessage", 4).await;
    for message in &sent[2..] {
        let text = message["text"].as_str().unwrap();
        assert!(
            text.starts_with("Coordinator ") && text.ends_with(" is paused until further notice."),
            "{text}"
        );
    }
    // Events are notified in order, so nothing earlier is still on its way.
    assert_eq!(texts(&fake).len(), 4, "{:?}", texts(&fake));
}

fn stalled_after() -> TimeDelta {
    TimeDelta::seconds(i64::try_from(TelegramConfig::default().stalled_after_secs).unwrap())
}

/// Switches autopilot on for `board`, with `A` holding its coordinator lease.
fn autopilot(t: &T, board: &str) {
    let expires = (t.state.now() + TimeDelta::days(365)).timestamp_millis();
    t.exec(&format!(
        "UPDATE boards SET autopilot = 1, coordinator = '{A}', \
         coordinator_expires_at = {expires} WHERE slug = '{board}'"
    ));
}

/// One check of the stalled boards, without sending: the boards due an alert.
async fn stalled_now(t: &T) -> Vec<String> {
    let quiet_ms = stalled_after().num_milliseconds();
    t.state
        .write(move |cx| crate::ops::telegram::stalled(cx, quiet_ms))
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.board)
        .collect()
}

#[tokio::test]
async fn a_stalled_autopilot_board_is_reported_once_until_a_claim() {
    let (fake, api_base) = fake_api().await;
    let t = T::with(|c| c.public_url = Some("https://board.example".to_owned()));
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    autopilot(&t, "factory");
    let first = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Ready work", "auto": true }),
        )
        .await;
    let mut notifier = Scheduled::start(&t, api_base, |c| c.stuck_after_secs = 0);
    // The first check sees the board stalled; the clock starts there.
    assert!(notifier.sent_after_check(&fake).await.is_empty());

    // Just under the threshold: nothing.
    t.advance(stalled_after() - TimeDelta::minutes(1));
    assert!(notifier.sent_after_check(&fake).await.is_empty());

    t.advance(TimeDelta::minutes(2));
    let sent = fake.wait("sendMessage", 1).await;
    assert_eq!(sent[0]["chat_id"], CHAT);
    let text = sent[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with(
            "Stalled: 1 autopilot board with ready work and nothing claimed for 2 h 0 min or \
             more.\n\nfactory: 1 ready auto task, no task in doing, stalled for 2 h 1 min \
             (since "
        ),
        "{text}"
    );
    assert!(text.contains(").\nCoordinator t3:"), "{text}");
    assert!(text.contains(", last seen "), "{text}");
    assert!(
        text.ends_with("https://board.example/boards/factory"),
        "{text}"
    );
    // Still stalled: no repeat.
    t.advance(stalled_after() * 2);
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);

    // A claim ends it; work in doing never stalls, however long it takes.
    assert!(t.claim(A, &first).await.0.is_success());
    let second = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "More work", "auto": true }),
        )
        .await;
    t.advance(stalled_after() * 3);
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);

    // Once the work is done and the rest sits unclaimed again, it is reported again.
    assert!(t.done(A, &first, super::check(0)).await.0.is_success());
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
    t.advance(stalled_after() - TimeDelta::minutes(1));
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
    t.advance(TimeDelta::minutes(2));
    let sent = fake.wait("sendMessage", 2).await;
    let text = sent[1]["text"].as_str().unwrap();
    assert!(text.contains("\nfactory: 1 ready auto task,"), "{text}");
    assert!(
        !text.contains(&second),
        "tasks are counted, not listed: {text}"
    );
}

#[tokio::test]
async fn a_board_stalls_from_when_its_work_became_claimable() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    // Ready auto work, but no autopilot yet.
    t.board("manual", &[]).await;
    t.task_with(
        A,
        json!({ "board": "manual", "title": "Ready", "auto": true }),
    )
    .await;
    // Autopilot, but only work that waits on the user.
    t.board("waits", &[]).await;
    autopilot(&t, "waits");
    t.task_with(
        A,
        json!({ "board": "waits", "title": "Decide", "auto": false }),
    )
    .await;
    // Autopilot and ready auto work, but a task in doing.
    t.board("busy", &[]).await;
    autopilot(&t, "busy");
    let doing = t.task(A, "busy", "In progress").await;
    assert!(t.claim(A, &doing).await.0.is_success());
    t.task_with(A, json!({ "board": "busy", "title": "Next", "auto": true }))
        .await;
    // Autopilot and an empty board.
    t.board("fresh", &[]).await;
    autopilot(&t, "fresh");
    let mut notifier = Scheduled::start(&t, api_base, |c| c.stuck_after_secs = 0);

    t.advance(stalled_after() * 3);
    assert!(notifier.sent_after_check(&fake).await.is_empty());

    // Work that just became claimable is the coordinator's to claim first, however long
    // the board was quiet or the work waited: a new task, and old work on a board that
    // just switched autopilot on.
    t.task_with(A, json!({ "board": "fresh", "title": "New", "auto": true }))
        .await;
    autopilot(&t, "manual");
    assert!(notifier.sent_after_check(&fake).await.is_empty());
    t.advance(stalled_after() - TimeDelta::minutes(1));
    assert!(notifier.sent_after_check(&fake).await.is_empty());
    t.advance(TimeDelta::minutes(2));
    let sent = fake.wait("sendMessage", 1).await;
    let text = sent[0]["text"].as_str().unwrap();
    assert!(text.starts_with("Stalled: 2 autopilot boards "), "{text}");
    assert!(text.contains("\nfresh: 1 ready auto task,"), "{text}");
    assert!(text.contains("\nmanual: 1 ready auto task,"), "{text}");
}

#[tokio::test]
async fn a_move_through_doing_between_checks_restarts_the_clock() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    autopilot(&t, "factory");
    let id = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Ready", "auto": true }),
        )
        .await;
    assert!(stalled_now(&t).await.is_empty());

    // Into doing (here without an event) and out by a status change, both between two
    // checks.
    t.advance(stalled_after() - TimeDelta::minutes(1));
    t.exec(&format!(
        "UPDATE tasks SET status = 'doing', owner = '{A}' WHERE id = '{id}'"
    ));
    let (s, v) = t.patch(A, &id, json!({ "status": "open" })).await;
    assert!(s.is_success(), "{v}");
    t.advance(TimeDelta::minutes(2));
    assert!(stalled_now(&t).await.is_empty());
    // A claim and a release do the same.
    t.advance(stalled_after() - TimeDelta::minutes(1));
    assert!(t.claim(A, &id).await.0.is_success());
    t.ok(
        "POST",
        &format!("/v1/tasks/{id}/release"),
        Some(A),
        Some(json!({ "agent": A })),
    )
    .await;
    t.advance(TimeDelta::minutes(2));
    assert!(stalled_now(&t).await.is_empty());
    t.advance(stalled_after());
    assert_eq!(stalled_now(&t).await, ["factory"]);
}

#[tokio::test]
async fn a_paused_or_absent_coordinator_decides_whether_a_board_stalls() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    autopilot(&t, "factory");
    t.task_with(
        A,
        json!({ "board": "factory", "title": "Ready", "auto": true }),
    )
    .await;
    // The coordinator stops on a usage limit until further notice: its pause has its own
    // message, so the board does not stall.
    t.exec(&format!(
        "UPDATE agents SET role = 'coordinator', state = 'paused', paused_until = NULL \
         WHERE id = '{A}'"
    ));
    assert!(stalled_now(&t).await.is_empty());
    t.advance(stalled_after() * 2);
    assert!(stalled_now(&t).await.is_empty());

    // Its lease runs out: no coordinator holds the board, so the clock starts.
    let expired = t.state.now().timestamp_millis();
    t.exec(&format!(
        "UPDATE boards SET coordinator_expires_at = {expired} WHERE slug = 'factory'"
    ));
    assert!(stalled_now(&t).await.is_empty());
    t.advance(stalled_after() + TimeDelta::minutes(1));
    let quiet_ms = stalled_after().num_milliseconds();
    let due = t
        .state
        .write(move |cx| crate::ops::telegram::stalled(cx, quiet_ms))
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].coordinator, None);
}

#[tokio::test]
async fn a_coordinator_ended_during_a_limit_holds_its_board_until_the_reset() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    t.task_with(
        A,
        json!({ "board": "factory", "title": "Ready", "auto": true }),
    )
    .await;
    // T3 closed its session while it waited for the reset, and the daemon named it
    // coordinator again: it keeps the reset, so the board does not stall before it.
    let reset = t.state.now() + stalled_after() * 2;
    pause(&t, A, Some(reset)).await;
    end(&t, A).await;
    autopilot(&t, "factory");
    assert!(stalled_now(&t).await.is_empty());
    t.advance(stalled_after() * 3 / 2);
    assert!(stalled_now(&t).await.is_empty());

    // From the reset on, the clock runs.
    t.advance(stalled_after());
    assert!(stalled_now(&t).await.is_empty());
    t.advance(stalled_after() + TimeDelta::minutes(1));
    assert_eq!(stalled_now(&t).await, ["factory"]);
}

#[tokio::test]
async fn a_new_chat_gets_the_stalled_alert_and_a_long_list_continues() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    let slug = |n: usize| format!("{}-{n:02}", "s".repeat(36));
    for n in 0..30 {
        t.board(&slug(n), &[]).await;
        autopilot(&t, &slug(n));
        t.task_with(
            A,
            json!({ "board": slug(n), "title": "Ready", "auto": true }),
        )
        .await;
    }
    let mut notifier = Scheduled::start(&t, api_base, |c| c.stuck_after_secs = 0);
    assert!(notifier.sent_after_check(&fake).await.is_empty());

    t.advance(stalled_after() + TimeDelta::minutes(1));
    let sent = fake.wait("sendMessage", 2).await;
    let listed = |text: &str| {
        text.lines()
            .filter(|l| l.starts_with("ssss"))
            .map(|l| l.split(':').next().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let first = sent[0]["text"].as_str().unwrap();
    assert!(
        first.starts_with("Stalled: 30 autopilot boards "),
        "{first}"
    );
    assert!(first.chars().count() <= 4096, "{first}");
    let shown = listed(first);
    assert!(!shown.is_empty() && shown.len() < 30, "{first}");
    assert!(
        first.ends_with(&format!(
            "… and {} more in the next message.",
            30 - shown.len()
        )),
        "{first}"
    );
    let mut all: Vec<_> = shown
        .into_iter()
        .chain(listed(sent[1]["text"].as_str().unwrap()))
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 30);
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 2);

    // A new chat is paired: it gets the alert too, without waiting the threshold again.
    let code = t
        .state
        .write(|cx| crate::ops::telegram::create_code(cx, true))
        .await
        .unwrap();
    assert!(
        t.state
            .write(move |cx| crate::ops::telegram::redeem(cx, &code, 7))
            .await
            .unwrap()
    );
    assert_eq!(stalled_now(&t).await.len(), 30);
}

#[tokio::test]
async fn the_digest_goes_out_once_a_day_at_the_set_time() {
    let (fake, api_base) = fake_api().await;
    let t = T::with(|c| c.public_url = Some("https://board.example".to_owned()));
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    t.board("zeta", &[]).await;
    let asked = t.task(A, "factory", "Pick a licence").await;
    let ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "factory", "task": asked, "kind": "decision",
                "question": "MIT or Apache?\nDetails follow.", "options": ["MIT", "Apache"],
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let waits = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Sign the form", "auto": false }),
        )
        .await;
    let proposal = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Cache builds", "auto": false,
                    "body": "Proposal: cache the builds." }),
        )
        .await;
    let ready = t
        .task_with(
            A,
            json!({ "board": "factory", "title": "Ordinary work", "auto": true }),
        )
        .await;
    let blocked = t.task(A, "zeta", "Renew the domain").await;
    let (s, v) = t
        .patch(
            A,
            &blocked,
            json!({ "status": "blocked", "blocked_reason": "waits on the registrar" }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let zeta_ask = t
        .ok(
            "POST",
            "/v1/asks",
            Some(A),
            Some(json!({
                "agent": A, "board": "zeta", "kind": "action",
                "question": "Log in to the registrar.",
            })),
        )
        .await["id"]
        .as_i64()
        .unwrap();

    // 12:00 UTC is 14:00 at +02:00; the digest is set for 15:00 there.
    let mut notifier = Scheduled::start(&t, api_base, |c| {
        c.digest_at = chrono::NaiveTime::from_hms_opt(15, 0, 0);
        c.utc_offset = "+02:00".parse().unwrap();
    });
    assert!(notifier.sent_after_check(&fake).await.is_empty());

    t.advance(TimeDelta::hours(1));
    let sent = fake.wait("sendMessage", 1).await;
    let text = sent[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Needs you, 2026-11-06: 2 open asks."),
        "{text}"
    );
    let factory = text.find("\nfactory\n").expect(text);
    let zeta = text.find("\nzeta\n").expect(text);
    assert!(factory < zeta, "{text}");
    assert!(
        text.contains(&format!("· ask {ask}: MIT or Apache?\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!("· ask {zeta_ask}: Log in to the registrar.\n")),
        "{text}"
    );
    // Only asks wait on the user: a task is never listed, not even blocked or non-auto ones.
    for id in [&asked, &waits, &proposal, &ready, &blocked] {
        assert!(!text.contains(&format!("· {id} ")), "{id}: {text}");
    }
    assert!(text.ends_with("https://board.example/asks"), "{text}");
    // Once a day.
    t.advance(TimeDelta::hours(8));
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
    // The next local day, at 15:00 again.
    t.advance(TimeDelta::hours(14));
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
    t.advance(TimeDelta::hours(2));
    let sent = fake.wait("sendMessage", 2).await;
    assert!(
        sent[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("Needs you, 2026-11-07:"),
        "{}",
        sent[1]["text"]
    );
}

#[tokio::test]
async fn an_empty_digest_is_skipped() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    t.task_with(
        A,
        json!({ "board": "factory", "title": "Ordinary work", "auto": true }),
    )
    .await;
    // Tasks without an ask do not wait on the user, so they send nothing either.
    t.task_with(
        A,
        json!({ "board": "factory", "title": "Sign the form", "auto": false }),
    )
    .await;
    let blocked = t.task(A, "factory", "Renew the domain").await;
    let (s, v) = t
        .patch(
            A,
            &blocked,
            json!({ "status": "blocked", "blocked_reason": "waits on the registrar" }),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let mut notifier = Scheduled::start(&t, api_base, |c| {
        c.digest_at = chrono::NaiveTime::from_hms_opt(9, 0, 0);
    });
    t.advance(TimeDelta::days(1));
    assert_eq!(notifier.sent_after_check(&fake).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_long_digest_counts_the_items_it_leaves_out() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    let long = "x".repeat(130);
    for board in ["factory", "zeta"] {
        t.board(board, &[]).await;
        for _ in 0..20 {
            t.ok(
                "POST",
                "/v1/asks",
                Some(A),
                Some(json!({
                    "agent": A, "board": board, "kind": "action", "question": long,
                })),
            )
            .await;
        }
    }
    let mut notifier = Scheduled::start(&t, api_base, |c| {
        c.digest_at = chrono::NaiveTime::from_hms_opt(9, 0, 0);
    });
    t.advance(TimeDelta::days(1));
    let sent = notifier.sent_after_check(&fake).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let text = &sent[0];
    assert!(text.chars().count() <= 4096, "{text}");
    let shown = text.lines().filter(|l| l.starts_with("· ")).count();
    assert!(shown > 0 && shown < 40, "{text}");
    // Board headings are not items.
    assert!(
        text.ends_with(&format!("… and {} more.", 40 - shown)),
        "{text}"
    );
}

#[tokio::test]
async fn a_digest_due_just_before_midnight_still_goes_out_after_it() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    ask(&t, "Sign the form").await;
    // The clock starts at 12:00 UTC on 2026-11-06; the digest is due at 23:59.
    let mut notifier = Scheduled::start(&t, api_base, |c| {
        c.digest_at = chrono::NaiveTime::from_hms_opt(23, 59, 0);
    });
    assert!(notifier.sent_after_check(&fake).await.is_empty());
    // The first check after 23:59 runs at 00:00:30 on the next day.
    t.advance(TimeDelta::hours(12) + TimeDelta::seconds(30));
    let sent = fake.wait("sendMessage", 1).await;
    let text = sent[0]["text"].as_str().unwrap();
    assert!(text.starts_with("Needs you, 2026-11-06: "), "{text}");
    // Once only, and not again before the next 23:59.
    t.advance(TimeDelta::hours(2));
    assert_eq!(notifier.sent_after_check(&fake).await.len(), 1);
}

#[tokio::test]
async fn the_digest_counts_tasks_done_since_last_seen() {
    let (fake, api_base) = fake_api().await;
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    let before = t.task(A, "factory", "Done before").await;
    assert!(t.claim(A, &before).await.0.is_success());
    assert!(t.done(A, &before, super::check(0)).await.0.is_success());
    t.advance(TimeDelta::minutes(1));
    let (s, v) = t
        .as_user(
            "PUT",
            "/v1/updates/seen",
            Some(json!({ "at": super::iso(t.state.now()) })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    t.advance(TimeDelta::minutes(1));
    for title in ["First after", "Second after"] {
        let id = t.task(A, "factory", title).await;
        assert!(t.claim(A, &id).await.0.is_success());
        assert!(t.done(A, &id, super::check(0)).await.0.is_success());
    }
    let mut notifier = Scheduled::start(&t, api_base, |c| {
        c.digest_at = chrono::NaiveTime::from_hms_opt(9, 0, 0);
    });
    t.advance(TimeDelta::days(1));
    let sent = notifier.sent_after_check(&fake).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].ends_with("\nDone since you last looked: 2 tasks."),
        "{}",
        sent[0]
    );
}
