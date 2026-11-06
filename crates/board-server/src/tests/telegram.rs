use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::T;
use crate::{TelegramConfig, spawn_telegram};

const CHAT: i64 = 4242;
const TOKEN: &str = "123:test-token";
const A: &str = "t3:alpha";

/// A fake Bot API: records every call and hands out queued updates.
#[derive(Clone, Default)]
struct Fake {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    updates: Arc<Mutex<VecDeque<Value>>>,
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
    fake.calls.lock().unwrap().push((method.clone(), body));
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

#[tokio::test]
async fn asks_round_trip_through_telegram() {
    let fake = Fake::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    let server = Router::new()
        .route("/{bot}/{method}", post(bot_api))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });

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
