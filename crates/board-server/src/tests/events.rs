use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::T;

const A: &str = "t3:alpha";

#[tokio::test]
async fn event_pages_follow_since_limit_and_board() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    t.board("other", &[]).await;
    t.task(A, "factory", "One").await;
    t.task(A, "other", "Two").await;
    let all = t.ok("GET", "/v1/events", None, None).await;
    let seqs: Vec<i64> = all["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(all["next_since"], *seqs.last().unwrap());
    assert_eq!(
        t.event_types(0).await,
        [
            "agent_registered",
            "board_created",
            "board_created",
            "board_joined",
            "task_created",
            "board_joined",
            "task_created",
        ]
    );

    let page = t.ok("GET", "/v1/events?since=0&limit=2", None, None).await;
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_since"], seqs[1]);
    let rest = t
        .ok("GET", &format!("/v1/events?since={}", seqs[1]), None, None)
        .await;
    assert_eq!(rest["events"][0]["seq"], seqs[2]);

    let factory = t.ok("GET", "/v1/events?board=factory", None, None).await;
    let types: Vec<&str> = factory["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["board_created", "board_joined", "task_created"]);
    assert_eq!(factory["events"][2]["payload"]["title"], "One");

    let empty = t.ok("GET", "/v1/events?since=9999", None, None).await;
    assert_eq!(
        (empty["events"].clone(), empty["next_since"].as_i64()),
        (json!([]), Some(9999))
    );
}

/// Reads server-sent events from `body` until `count` have arrived.
async fn read_events(
    body: &mut (impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
    buffer: &mut String,
    count: usize,
) -> Vec<(i64, Value)> {
    let mut out = Vec::new();
    while out.len() < count {
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let id = frame
                .lines()
                .find_map(|l| l.strip_prefix("id:"))
                .map(|v| v.trim().parse::<i64>().unwrap());
            let data = frame
                .lines()
                .find_map(|l| l.strip_prefix("data:"))
                .map(|v| serde_json::from_str::<Value>(v.trim()).unwrap());
            if let (Some(id), Some(data)) = (id, data) {
                out.push((id, data));
            }
        }
        if out.len() >= count {
            break;
        }
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("an event within 5s")
            .expect("stream open")
            .unwrap();
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    out
}

#[tokio::test]
async fn event_stream_resumes_from_last_event_id() {
    let t = T::new();
    t.register(A, None, None).await;
    t.board("factory", &[]).await;
    t.task(A, "factory", "One").await;
    t.task(A, "factory", "Two").await;
    let seqs: Vec<i64> = t.ok("GET", "/v1/events", None, None).await["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs.len(), 5);

    let request = Request::get("/v1/events?since=0")
        .header("authorization", format!("Bearer {}", t.token))
        .header("accept", "text/event-stream")
        .header("last-event-id", seqs[2].to_string())
        .body(Body::empty())
        .unwrap();
    let response = t.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut body = response.into_body().into_data_stream();
    let mut buffer = String::new();

    // Last-Event-ID wins over since: the backlog starts after it.
    let backlog = read_events(&mut body, &mut buffer, 2).await;
    assert_eq!(
        backlog.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        seqs[3..]
    );
    assert_eq!(backlog[0].1["payload"]["title"], "One");
    assert_eq!(backlog[0].1["seq"], backlog[0].0);

    // Live events follow, once each.
    t.task(A, "factory", "Three").await;
    let live = read_events(&mut body, &mut buffer, 1).await;
    assert!(live[0].0 > seqs[4]);
    assert_eq!(live[0].1["payload"]["title"], "Three");

    // Shutdown ends the stream.
    t.state.shutdown();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = body.next().await {
            chunk.unwrap();
        }
    })
    .await;
    assert!(end.is_ok(), "the stream ends on shutdown");

    let (s, v) = t
        .send(
            "GET",
            "/v1/events",
            &[
                ("authorization", &format!("Bearer {}", t.token)),
                ("accept", "text/event-stream"),
                ("last-event-id", "abc"),
            ],
            None,
        )
        .await;
    assert_eq!((s.as_u16(), v["code"].as_str()), (400, Some("invalid")));
}
