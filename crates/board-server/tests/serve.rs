//! `serve` runs until SIGTERM and then shuts down gracefully. Alone in its own test binary,
//! because the signal goes to the whole process.

#![cfg(unix)]

use std::time::Duration;

use board_server::{ServerConfig, create_device, serve};

#[tokio::test]
async fn serve_stops_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("board.db");
    let token = create_device(&db_path, &"mac".parse().unwrap(), false).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut config = ServerConfig::new(&db_path);
    config.listen = ([127, 0, 0, 1], port).into();
    let server = tokio::spawn(serve(config));

    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while http.get(format!("{base}/healthz")).send().await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the server starts");

    // An open event stream must not hold the shutdown up.
    let mut stream = http
        .get(format!("{base}/v1/events"))
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status().as_u16(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let status = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("serve returns after SIGTERM")
        .unwrap();
    result.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the event stream ends");
}
