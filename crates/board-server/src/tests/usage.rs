use chrono::{SecondsFormat, TimeDelta};
use serde_json::{Value, json};

use super::{REPO, T, start};

const ME: &str = "t3:me";

fn at(minutes: i64) -> String {
    (start() + TimeDelta::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A report read `minutes` after the start, with the given 5h and weekly percents.
fn reading(minutes: i64, five_hour: f64, week: f64) -> Value {
    json!({
        "five_hour": { "percent": five_hour, "resets_at": at(minutes + 120) },
        "seven_day": { "percent": week, "resets_at": at(minutes + 3 * 24 * 60) },
        "read_at": at(minutes),
    })
}

async fn snapshot(t: &T, query: &str) -> String {
    t.ok("GET", &format!("/v1/snapshot?{query}"), Some(ME), None)
        .await["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn usage_line(snapshot: &str) -> Option<&str> {
    snapshot.lines().find(|l| l.starts_with("usage: "))
}

#[tokio::test]
async fn devices_report_usage_and_the_newest_reading_wins() {
    let t = T::new();
    assert_eq!(t.ok("GET", "/v1/usage", None, None).await, Value::Null);

    let v = t
        .ok("PUT", "/v1/usage", None, Some(reading(0, 12.0, 74.0)))
        .await;
    assert_eq!(v["device"], "mac");
    assert_eq!(v["seven_day"]["percent"], 74.0);

    // An older reading, say from a device whose poll was slow, does not replace it.
    let v = t
        .ok("PUT", "/v1/usage", None, Some(reading(-5, 99.0, 99.0)))
        .await;
    assert_eq!(v["seven_day"]["percent"], 74.0, "{v}");

    let v = t
        .ok("PUT", "/v1/usage", None, Some(reading(3, 20.0, 75.0)))
        .await;
    assert_eq!(v["seven_day"]["percent"], 75.0, "{v}");
    let got = t.ok("GET", "/v1/usage", None, None).await;
    assert_eq!(got, v);

    // A reading may leave out a window, but needs its read time.
    let v = t
        .ok(
            "PUT",
            "/v1/usage",
            None,
            Some(json!({ "read_at": at(4), "seven_day": { "percent": 76.5 } })),
        )
        .await;
    assert!(v.get("five_hour").is_none(), "{v}");
    assert_eq!(v["seven_day"]["percent"], 76.5);
}

#[tokio::test]
async fn usage_reports_are_validated() {
    let t = T::new();
    for body in [
        reading(0, -1.0, 10.0),
        reading(0, 10.0, 1000.5),
        // Further ahead of the server's clock than the allowed skew.
        reading(6, 10.0, 10.0),
        json!({ "seven_day": { "percent": 10.0 } }),
    ] {
        let (code, err, v) = t.fail("PUT", "/v1/usage", None, Some(body)).await;
        assert!(
            (code, err.as_str()) == (400, "invalid") || code == 422,
            "{v}"
        );
    }
    // A small skew is fine.
    t.ok("PUT", "/v1/usage", None, Some(reading(4, 10.0, 10.0)))
        .await;

    // The reading names the device that sent it: here the user's device.
    let (s, v) = t
        .as_user("PUT", "/v1/usage", Some(reading(5, 10.0, 11.0)))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["device"], "phone");
    let (s, v) = t.as_user("GET", "/v1/usage", None).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["seven_day"]["percent"], 11.0);
}

#[tokio::test]
async fn only_the_user_raises_usage_stop_at_and_anyone_lowers_it() {
    let t = T::new();
    let board = t.board("factory", &[]).await;
    assert_eq!(board["usage_stop_at"], 90);

    for n in [0, 101] {
        let (code, err, v) = t
            .fail(
                "PATCH",
                "/v1/boards/factory",
                None,
                Some(json!({ "usage_stop_at": n })),
            )
            .await;
        assert_eq!((code, err.as_str()), (400, "invalid"), "{v}");
        assert!(
            v["next"].as_str().unwrap().contains("between 1 and 100"),
            "{v}"
        );
    }

    let v = t
        .ok(
            "PATCH",
            "/v1/boards/factory",
            None,
            Some(json!({ "usage_stop_at": 60 })),
        )
        .await;
    assert_eq!(v["usage_stop_at"], 60);
    let page = t
        .ok("GET", "/v1/events?since=0&limit=1000", None, None)
        .await;
    let last = page["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["payload"]["fields"], json!(["usage_stop_at"]));

    let (code, err, v) = t
        .fail(
            "PATCH",
            "/v1/boards/factory",
            None,
            Some(json!({ "usage_stop_at": 70 })),
        )
        .await;
    assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");

    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "usage_stop_at": 95 })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["usage_stop_at"], 95);
}

#[tokio::test]
async fn snapshots_show_the_usage_and_say_to_stop_while_it_is_over() {
    let t = T::new();
    t.register(ME, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;

    // No reading yet: no line.
    assert_eq!(usage_line(&snapshot(&t, "kind=full").await), None);

    t.ok("PUT", "/v1/usage", None, Some(reading(0, 12.4, 74.9)))
        .await;
    t.advance(TimeDelta::minutes(3));
    let full = snapshot(&t, "kind=full").await;
    assert_eq!(
        usage_line(&full),
        Some("usage: 5h 12% · week 74% · read 3m ago · this board starts no new task at 90%"),
        "{full}"
    );
    // Under the stop, deltas and alerts stay quiet.
    assert_eq!(snapshot(&t, "kind=delta").await, "");
    assert_eq!(snapshot(&t, "kind=alerts").await, "");

    // Over the stop: every kind carries the line, with what to do and when it resets.
    t.ok("PUT", "/v1/usage", None, Some(reading(3, 20.0, 92.0)))
        .await;
    let over = format!(
        "usage: week 92% >= this board's stop at 90% -> usage is high: start no new task or \
         child thread; finish or park the tasks in doing. The week window resets {}.",
        (start() + TimeDelta::minutes(3 + 3 * 24 * 60)).format("%Y-%m-%d %H:%MZ")
    );
    for kind in ["full", "delta", "alerts"] {
        let snap = snapshot(&t, &format!("kind={kind}")).await;
        assert_eq!(usage_line(&snap), Some(over.as_str()), "{kind}:\n{snap}");
    }
    let yaml = snapshot(&t, "kind=full&format=yaml").await;
    assert!(
        yaml.contains("usage:\n  five_hour: 20\n  week: 92\n"),
        "{yaml}"
    );
    assert!(
        yaml.contains("  stop_at: 90\n  over:\n    window: week\n    percent: 92\n"),
        "{yaml}"
    );

    // The board's own stop decides: a board that may use more is not held.
    t.as_user(
        "PATCH",
        "/v1/boards/factory",
        Some(json!({ "usage_stop_at": 95 })),
    )
    .await;
    let full = snapshot(&t, "kind=full").await;
    assert!(
        usage_line(&full)
            .unwrap()
            .ends_with("starts no new task at 95%"),
        "{full}"
    );
    t.ok(
        "PATCH",
        "/v1/boards/factory",
        None,
        Some(json!({ "usage_stop_at": 90 })),
    )
    .await;

    // A stale reading holds nothing: the line clears from deltas and alerts.
    t.advance(TimeDelta::minutes(31));
    let full = snapshot(&t, "kind=full").await;
    assert!(
        usage_line(&full).unwrap().contains("read 31m ago"),
        "{full}"
    );
    assert_eq!(usage_line(&snapshot(&t, "kind=alerts").await), None);

    // A fresh reading under the stop clears it too.
    t.ok("PUT", "/v1/usage", None, Some(reading(34, 1.0, 10.0)))
        .await;
    assert_eq!(usage_line(&snapshot(&t, "kind=alerts").await), None);
}

#[tokio::test]
async fn a_window_past_its_reset_is_not_over() {
    let t = T::new();
    t.register(ME, Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    t.ok(
        "PUT",
        "/v1/usage",
        None,
        Some(json!({
            "five_hour": { "percent": 99.0, "resets_at": at(10) },
            "read_at": at(0),
        })),
    )
    .await;
    let snap = snapshot(&t, "kind=alerts").await;
    assert!(
        usage_line(&snap).unwrap().starts_with("usage: 5h 99% >="),
        "{snap}"
    );
    t.advance(TimeDelta::minutes(11));
    assert_eq!(usage_line(&snapshot(&t, "kind=alerts").await), None);
}
