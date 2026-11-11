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
        // Stale already: the device's clock is behind.
        reading(-31, 10.0, 10.0),
        // No window: it would release every board the last reading holds.
        json!({ "read_at": at(0) }),
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

    // Over the stop: full snapshots and deltas carry the line, with what to do and when it
    // resets. Alerts, which come with tool calls under the smallest cap, never do.
    t.ok("PUT", "/v1/usage", None, Some(reading(3, 20.0, 92.0)))
        .await;
    let over = format!(
        "usage: week 92% >= this board's stop at 90% -> usage is high: start no new task or \
         child thread; finish or park the tasks in doing. The week window resets {}.",
        (start() + TimeDelta::minutes(3 + 3 * 24 * 60)).format("%Y-%m-%d %H:%MZ")
    );
    for kind in ["full", "delta"] {
        let snap = snapshot(&t, &format!("kind={kind}")).await;
        assert_eq!(usage_line(&snap), Some(over.as_str()), "{kind}:\n{snap}");
    }
    assert_eq!(snapshot(&t, "kind=alerts").await, "");
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

    // A stale reading holds nothing: the line clears from deltas.
    t.advance(TimeDelta::minutes(31));
    let full = snapshot(&t, "kind=full").await;
    assert!(
        usage_line(&full).unwrap().contains("read 31m ago"),
        "{full}"
    );
    assert_eq!(usage_line(&snapshot(&t, "kind=delta").await), None);

    // A fresh reading under the stop clears it too.
    t.ok("PUT", "/v1/usage", None, Some(reading(34, 99.0, 10.0)))
        .await;
    assert!(usage_line(&snapshot(&t, "kind=delta").await).is_some());
    t.ok("PUT", "/v1/usage", None, Some(reading(35, 1.0, 10.0)))
        .await;
    assert_eq!(usage_line(&snapshot(&t, "kind=delta").await), None);
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
    let snap = snapshot(&t, "kind=delta").await;
    assert!(
        usage_line(&snap).unwrap().starts_with("usage: 5h 99% >="),
        "{snap}"
    );
    t.advance(TimeDelta::minutes(11));
    assert_eq!(usage_line(&snapshot(&t, "kind=delta").await), None);
}

/// A report read `minutes` after the start with only a weekly window that resets
/// `reset` minutes after the start.
fn week_reading(minutes: i64, week: f64, reset: i64) -> Value {
    json!({
        "seven_day": { "percent": week, "resets_at": at(reset) },
        "read_at": at(minutes),
    })
}

fn you_line(snapshot: &str) -> Option<&str> {
    snapshot.lines().find(|l| l.starts_with("you: "))
}

#[tokio::test]
async fn only_the_user_lowers_the_usage_reserve_or_turns_it_off() {
    let t = T::new();
    t.register(ME, Some(REPO), None).await;
    let reserve = |r: Value| Some(json!({ "reserve": r }));
    assert_eq!(
        t.ok("GET", "/v1/usage/reserve", Some(ME), None).await,
        json!({ "reserve": null })
    );
    // Anyone turns it on or raises it.
    t.ok("PUT", "/v1/usage/reserve", Some(ME), reserve(json!(5.5)))
        .await;
    t.ok("PUT", "/v1/usage/reserve", Some(ME), reserve(json!(6.0)))
        .await;
    for r in [json!(5.5), Value::Null] {
        let (code, err, v) = t
            .fail("PUT", "/v1/usage/reserve", Some(ME), reserve(r))
            .await;
        assert_eq!((code, err.as_str()), (403, "forbidden"), "{v}");
    }
    for r in [json!(-1.0), json!(100.0)] {
        let (code, _, v) = t
            .fail("PUT", "/v1/usage/reserve", Some(ME), reserve(r))
            .await;
        assert_eq!(code, 400, "{v}");
    }
    // A missing field is refused, not read as off.
    let (s, v) = t
        .as_user("PUT", "/v1/usage/reserve", Some(json!({ "reserv": 6.0 })))
        .await;
    assert!(s.is_client_error(), "{v}");
    assert_eq!(
        t.ok("GET", "/v1/usage/reserve", Some(ME), None).await,
        json!({ "reserve": 6.0 })
    );
    let (s, v) = t
        .as_user("PUT", "/v1/usage/reserve", reserve(json!(5.5)))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(
        t.ok("GET", "/v1/usage/reserve", Some(ME), None).await,
        json!({ "reserve": 5.5 })
    );
    let (s, v) = t
        .as_user("PUT", "/v1/usage/reserve", reserve(Value::Null))
        .await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v, json!({ "reserve": null }));
}

#[tokio::test]
async fn the_reserve_paces_each_day_and_the_hard_line_holds_working_agents() {
    let t = T::new();
    t.register(ME, Some(REPO), None).await;
    t.register("t3:other", Some(REPO), None).await;
    t.board("factory", &[REPO]).await;
    // The other agent coordinated a board that is closed now.
    t.board("old", &[REPO]).await;
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/old",
            Some(json!({ "autopilot": true })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    t.ok(
        "POST",
        "/v1/boards/old/coordinator",
        Some("t3:other"),
        Some(json!({ "device": "mac", "agent": "t3:other" })),
    )
    .await;
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/old",
            Some(json!({ "status": "closed" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "parallel_tasks": 3, "autopilot": true })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    t.ok(
        "POST",
        "/v1/boards/factory/coordinator",
        Some(ME),
        Some(json!({ "device": "mac", "agent": ME })),
    )
    .await;
    // Three pace days to the reset: this one began 12 hours before the start.
    let reset = (2 * 24 + 12) * 60;
    t.ok("PUT", "/v1/usage", None, Some(week_reading(0, 80.0, reset)))
        .await;
    let full = snapshot(&t, "kind=full").await;
    assert!(!usage_line(&full).unwrap().contains("pace"), "off: {full}");

    t.ok(
        "PUT",
        "/v1/usage/reserve",
        Some(ME),
        Some(json!({ "reserve": 5.5 })),
    )
    .await;
    // The day's line: 80 + (94.5 - 80) / 3.
    let pace = t.ok("GET", "/v1/usage", None, None).await["pace"].clone();
    assert_eq!(pace["anchor"], 80.0);
    assert_eq!(pace["day_start"], at(-12 * 60));
    assert!(
        (pace["line"].as_f64().unwrap() - 84.833).abs() < 0.001,
        "{pace}"
    );
    assert_eq!(pace["projected"], 80.0, "no burn rate yet");
    let full = snapshot(&t, "kind=full").await;
    assert!(
        usage_line(&full).unwrap().ends_with(
            "pace: today to 84.8% (allows 4.8), projected 80%, up to 3 tasks at once, \
             reserve 5.5%"
        ),
        "{full}"
    );
    assert!(
        you_line(&full)
            .unwrap()
            .ends_with("coordinator, up to 3 tasks at once"),
        "{full}"
    );

    // 2.5 points in 15 minutes projects 83.3 in 5 more: within 2 points, one at a time.
    // The reset time moved by a fraction of a second: the same day, the same anchor.
    t.advance(TimeDelta::minutes(15));
    let mut reading = week_reading(15, 82.5, reset);
    reading["seven_day"]["resets_at"] = json!(
        (start() + TimeDelta::minutes(reset) + TimeDelta::milliseconds(400))
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    );
    t.ok("PUT", "/v1/usage", None, Some(reading)).await;
    let pace = t.ok("GET", "/v1/usage", None, None).await["pace"].clone();
    assert_eq!(pace["anchor"], 80.0, "{pace}");
    let full = snapshot(&t, "kind=full").await;
    assert!(
        usage_line(&full)
            .unwrap()
            .contains("projected 83.3%, one task at a time"),
        "{full}"
    );
    assert!(
        you_line(&full)
            .unwrap()
            .ends_with("coordinator, one task at a time"),
        "{full}"
    );

    // Projected past the line: no new task, said in deltas too.
    t.advance(TimeDelta::minutes(15));
    t.ok(
        "PUT",
        "/v1/usage",
        None,
        Some(week_reading(30, 84.5, reset)),
    )
    .await;
    let stop = "usage: week projected 85.3% >= today's line 84.8% -> today's share of the \
                weekly usage is spent";
    for kind in ["full", "delta"] {
        let snap = snapshot(&t, &format!("kind={kind}")).await;
        assert!(
            usage_line(&snap).unwrap().starts_with(stop),
            "{kind}: {snap}"
        );
    }
    let full = snapshot(&t, "kind=full").await;
    assert!(
        you_line(&full)
            .unwrap()
            .ends_with("coordinator, no new task while the pace is 0"),
        "{full}"
    );

    // The next day re-anchors at its first reading: 84.6 + (94.5 - 84.6) / 2.
    t.advance(TimeDelta::minutes(12 * 60 - 25));
    t.ok(
        "PUT",
        "/v1/usage",
        None,
        Some(week_reading(12 * 60 + 5, 84.6, reset)),
    )
    .await;
    let pace = t.ok("GET", "/v1/usage", None, None).await["pace"].clone();
    assert_eq!(pace["day_start"], at(12 * 60));
    assert!(
        (pace["line"].as_f64().unwrap() - 89.55).abs() < 0.001,
        "{pace}"
    );
    assert_eq!(pace["projected"], 84.6, "older readings are dropped");

    // The hard line: agents working for a board halt; others do not.
    let id = t.task(ME, "factory", "Work").await;
    assert!(t.claim(ME, &id).await.0.is_success());
    t.advance(TimeDelta::minutes(5));
    t.ok(
        "PUT",
        "/v1/usage",
        None,
        Some(week_reading(12 * 60 + 10, 94.5, reset)),
    )
    .await;
    let snap = t
        .ok("GET", "/v1/snapshot?kind=alerts", Some(ME), None)
        .await;
    assert_eq!(snap["held_by"], json!(["factory"]), "{snap}");
    assert_eq!(snap["held_for_usage"], true);
    assert_eq!(snap["held_since"], at(12 * 60 + 10));
    assert!(
        snap["text"].as_str().unwrap().contains(
            "hold: factory: weekly usage 94.5% reached 94.5% (the user keeps 5.5% in reserve) \
             -> the weekly usage reached the user's reserve. If the user's own message in this \
             turn asks you to go on, such as \"continue\" or \"retry\", do that work: the hold \
             does not stop it. In every other turn, such as a wake-up or a child's report, \
             stop: end this turn now, and start no task, child thread or wake-up.\n"
        ),
        "{snap}"
    );
    let other = t
        .ok("GET", "/v1/snapshot?kind=alerts", Some("t3:other"), None)
        .await;
    assert_eq!(other["held_by"], Value::Null, "{other}");
    // A later reading keeps the time the line was reached.
    t.ok(
        "PUT",
        "/v1/usage",
        None,
        Some(week_reading(12 * 60 + 11, 95.0, reset)),
    )
    .await;
    let snap = t
        .ok("GET", "/v1/snapshot?kind=alerts", Some(ME), None)
        .await;
    assert_eq!(snap["held_since"], at(12 * 60 + 10));

    // A pause after the line dates the hold from the pause, so it ends turns begun since.
    t.advance(TimeDelta::minutes(2));
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "status": "paused" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let snap = t
        .ok("GET", "/v1/snapshot?kind=alerts", Some(ME), None)
        .await;
    assert_eq!(snap["held_since"], at(12 * 60 + 12), "{snap}");
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "status": "active" })),
        )
        .await;
    assert!(s.is_success(), "{v}");

    // The user lowers the reserve: the hold ends.
    let (s, v) = t
        .as_user("PUT", "/v1/usage/reserve", Some(json!({ "reserve": 4.0 })))
        .await;
    assert!(s.is_success(), "{v}");
    let snap = t
        .ok("GET", "/v1/snapshot?kind=alerts", Some(ME), None)
        .await;
    assert_eq!(snap["held_by"], Value::Null, "{snap}");
    assert!(snap.get("held_for_usage").is_none(), "{snap}");
}

#[tokio::test]
async fn the_usage_holds_and_stops_only_claude_agents() {
    let t = T::new();
    t.board("factory", &[REPO]).await;
    let others = ["opencode:ses_1", "codex:c1"];
    t.register(ME, Some(REPO), None).await;
    t.register_as(others[0], "opencode", Some(REPO), None).await;
    t.register_as(others[1], "codex", Some(REPO), None).await;
    for agent in [ME].iter().chain(&others) {
        let id = t.task(agent, "factory", "Work").await;
        assert!(t.claim(agent, &id).await.0.is_success());
    }
    let (s, v) = t
        .as_user("PUT", "/v1/usage/reserve", Some(json!({ "reserve": 5.5 })))
        .await;
    assert!(s.is_success(), "{v}");
    t.ok(
        "PUT",
        "/v1/usage",
        None,
        Some(week_reading(0, 95.0, 3 * 24 * 60)),
    )
    .await;

    let t = &t;
    let full = |agent: &'static str| async move {
        t.ok("GET", "/v1/snapshot?kind=full", Some(agent), None)
            .await
    };
    let claude = full(ME).await;
    assert_eq!(claude["held_by"], json!(["factory"]), "{claude}");
    assert_eq!(claude["held_for_usage"], true);
    assert!(
        usage_line(claude["text"].as_str().unwrap()).is_some(),
        "{claude}"
    );
    for agent in others {
        let snap = full(agent).await;
        assert_eq!(snap["held_by"], Value::Null, "{agent}: {snap}");
        let text = snap["text"].as_str().unwrap();
        assert!(!text.contains("hold:"), "{agent}: {text}");
        assert_eq!(usage_line(text), None, "{agent}: {text}");
    }

    // A paused board still holds every runtime.
    let (s, v) = t
        .as_user(
            "PATCH",
            "/v1/boards/factory",
            Some(json!({ "status": "paused" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    for agent in others {
        let snap = full(agent).await;
        assert_eq!(snap["held_by"], json!(["factory"]), "{agent}: {snap}");
        assert_eq!(snap["held_for_usage"], Value::Null, "{agent}: {snap}");
    }
}
