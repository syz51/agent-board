//! End to end: the real binary as server (`board serve`), host admin (`board admin`) and
//! client (`board login`, `board add`, `board ls`) against one database.

mod common;

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::Value;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not start listening");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serve_admin_and_client_work_together() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("board.db");
    let db_arg = db.to_str().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let home = dir.path().join("board-home");
    std::fs::create_dir_all(&home).unwrap();

    // Host admin: create device tokens before the server runs.
    let out = board(
        &home,
        &cwd,
        &[],
        &["admin", "device", DEVICE, "--db", db_arg],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let token = stdout(&out).trim().to_owned();
    assert!(token.len() >= 32, "token looks too short");
    assert!(stderr(&out).contains("shown once"));
    let again = board(
        &home,
        &cwd,
        &[],
        &["admin", "device", DEVICE, "--db", db_arg],
        "",
    );
    assert_eq!(
        again.status.code(),
        Some(1),
        "a live device name is refused"
    );

    let port = free_port();
    let config = dir.path().join("server.toml");
    std::fs::write(
        &config,
        format!("db = {db_arg:?}\nlisten = \"127.0.0.1:{port}\"\n"),
    )
    .unwrap();
    let server = Server(
        Command::new(env!("CARGO_BIN_EXE_board"))
            .args(["serve", "--config", config.to_str().unwrap()])
            .env("BOARD_LOG", "warn")
            .env_remove("BOARD_TELEGRAM_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait_for_port(port);
    let url = format!("http://127.0.0.1:{port}");

    // Client: log in with the token from stdin, then use the board.
    let mut login = Command::new(env!("CARGO_BIN_EXE_board"));
    login
        .args(["login", "--server", &url, "--device", DEVICE])
        .env("BOARD_HOME", &home)
        .env("BOARD_KEYCHAIN", "0")
        .env_remove("BOARD_TOKEN");
    let out = run_with_stdin(login, &format!("{token}\n"));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stdout(&out).contains(&token) && !stderr(&out).contains(&token));

    let client = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_board"));
        command
            .args(args)
            .current_dir(&cwd)
            .env("BOARD_HOME", &home)
            .env("BOARD_KEYCHAIN", "0")
            .env_remove("BOARD_TOKEN")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env("T3CODE_THREAD_ID", "thread-e2e");
        detached(&command, "")
    };
    let out = client(&["board", "create", "e2e", "--title", "End to end"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = client(&["add", "First task", "--board", "e2e"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = client(&["claim", "e2e-1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = client(&["ls", "--board", "e2e", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let tasks: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(tasks[0]["id"], "e2e-1");
    assert_eq!(tasks[0]["status"], "doing");
    assert_eq!(tasks[0]["owner"], "t3:thread-e2e");

    // The web view (or its placeholder) is served outside /v1.
    let mut web = TcpStream::connect(("127.0.0.1", port)).unwrap();
    use std::io::{Read as _, Write as _};
    web.write_all(b"GET /boards/e2e HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    web.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("agent-board"));

    // Host admin: a consistent backup while the server runs; never over an existing file.
    let backup = dir.path().join("backup.db");
    let backup_arg = backup.to_str().unwrap();
    let out = board(
        &home,
        &cwd,
        &[],
        &["admin", "backup", "--db", db_arg, "--out", backup_arg],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(std::fs::metadata(&backup).unwrap().len() > 0);
    let out = board(
        &home,
        &cwd,
        &[],
        &["admin", "backup", "--db", db_arg, "--out", backup_arg],
        "",
    );
    assert_eq!(out.status.code(), Some(1));

    // SIGTERM shuts the server down cleanly.
    let mut server = server;
    let pid = server.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let status = server.0.wait().unwrap();
    assert!(status.success(), "server exit: {status:?}");
}

#[test]
fn serve_reports_a_missing_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = board(
        dir.path(),
        dir.path(),
        &[],
        &[
            "serve",
            "--config",
            dir.path().join("nope.toml").to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("nope.toml"), "{}", stderr(&out));
}

fn run_with_stdin(mut command: Command, stdin: &str) -> std::process::Output {
    use std::io::Write as _;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn admin_telegram_pair_prints_a_one_time_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("board.db");
    let db_arg = db.to_str().unwrap();
    let home = dir.path().join("board-home");
    std::fs::create_dir_all(&home).unwrap();
    let pair = |args: &[&str]| {
        let mut all = vec!["admin", "telegram-pair", "--db", db_arg];
        all.extend(args);
        board(&home, dir.path(), &[], &all, "")
    };
    let out = pair(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let code = stdout(&out).trim().to_owned();
    assert_eq!(code.len(), 12);
    assert!(
        code.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    );
    assert!(stderr(&out).contains(&format!("/start {code}")));
    // Only its hash is stored.
    let stored = std::fs::read(&db).unwrap();
    assert!(!stored.windows(code.len()).any(|w| w == code.as_bytes()));
    // Another code may be created while no chat is paired.
    let out = pair(&["--replace"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_ne!(stdout(&out).trim(), code);
}
