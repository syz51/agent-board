//! `board lease` warnings name each overlapping lease's task, its status and its holder.

mod common;

use board_proto::{AgentState, Lease, LeaseResult, TaskDetail, TaskStatus};
use chrono::{TimeDelta, Utc};
use common::*;

const ME: &str = "t3:thread-1";
const PEER: &str = "t3:thread:delegated-task:command%3Amcp%3Apeer";

fn lease(id: i64, agent: &str, task: Option<&str>, glob: &str) -> Lease {
    Lease {
        id,
        agent: agent.parse().unwrap(),
        task: task.map(|t| t.parse().unwrap()),
        repo: "github.com/acme/web".into(),
        glob: glob.into(),
        expires_at: Utc::now() + TimeDelta::minutes(30),
        created_at: None,
    }
}

#[test]
fn overlaps_name_the_task_its_status_and_the_holders_handle() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = home_for(dir.path(), &stub.url);
    let repo = git_repo(dir.path(), "git@github.com:acme/web.git");

    stub.on(
        &format!("GET /v1/agents/{}", enc(ME)),
        agent_fixture(ME, DEVICE, AgentState::Running),
    );
    let mut peer = agent_fixture(PEER, DEVICE, AgentState::Idle);
    peer.handle = "t3:4600".into();
    stub.on(&format!("GET /v1/agents/{}", enc(PEER)), peer);
    let mut task = task_fixture("acme-7", "Review keeps leases");
    task.status = TaskStatus::Review;
    stub.on(
        "GET /v1/tasks/acme-7",
        TaskDetail {
            task,
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    // acme-9 and the other holder have no stub: their lookups fail, and the lease call
    // still succeeds, printing the bare ids.
    stub.on(
        "POST /v1/leases",
        LeaseResult {
            leases: vec![lease(90, ME, Some("acme-8"), "src/**")],
            conflicts: vec![
                lease(86, PEER, Some("acme-7"), "src/a.rs"),
                lease(87, "t3:gone", Some("acme-9"), "src/b.rs"),
                lease(88, PEER, None, "src/c.rs"),
            ],
            later_conflicts: vec![],
            warning: None,
        },
    );

    let out = board(
        &home,
        &repo,
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "src/**", "--task", "acme-8"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    let expected = [
        (
            "! overlaps earlier lease #86 github.com/acme/web \"src/a.rs\" for acme-7 (review) by t3:4600 expires in ",
            " -> earlier lease holds (t3:4600 on acme-7): stop or narrow your globs",
        ),
        (
            "! overlaps earlier lease #87 github.com/acme/web \"src/b.rs\" for acme-9 by t3:gone expires in ",
            " -> earlier lease holds (t3:gone on acme-9): stop or narrow your globs",
        ),
        (
            "! overlaps earlier lease #88 github.com/acme/web \"src/c.rs\" for no task by t3:4600 expires in ",
            " -> earlier lease holds (t3:4600): stop or narrow your globs",
        ),
    ];
    for (line, (prefix, suffix)) in lines[1..].iter().zip(expected) {
        assert!(line.starts_with(prefix), "{line}");
        assert!(line.ends_with(suffix), "{line}");
    }
    // One lookup per distinct task and holder.
    assert_eq!(stub.requests_to("GET /v1/tasks/acme-7").len(), 1);
    assert_eq!(
        stub.requests_to(&format!("GET /v1/agents/{}", enc(PEER)))
            .len(),
        1
    );
}

#[test]
fn renewing_an_earlier_lease_says_earlier_lease_holds_over_a_later_peer() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = home_for(dir.path(), &stub.url);
    let repo = git_repo(dir.path(), "git@github.com:acme/web.git");

    stub.on(
        &format!("GET /v1/agents/{}", enc(ME)),
        agent_fixture(ME, DEVICE, AgentState::Running),
    );
    let mut peer = agent_fixture(PEER, DEVICE, AgentState::Running);
    peer.handle = "t3:9999".into();
    stub.on(&format!("GET /v1/agents/{}", enc(PEER)), peer);
    let mut task = task_fixture("acme-54", "Later overlapping task");
    task.status = TaskStatus::Doing;
    stub.on(
        "GET /v1/tasks/acme-54",
        TaskDetail {
            task,
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    let now = Utc::now();
    let mut mine = lease(40, ME, Some("acme-43"), "src/a.rs");
    mine.created_at = Some(now - TimeDelta::minutes(10));
    let mut theirs = lease(55, PEER, Some("acme-54"), "src/**");
    theirs.created_at = Some(now - TimeDelta::minutes(2));
    stub.on(
        "POST /v1/leases",
        LeaseResult {
            leases: vec![mine],
            conflicts: vec![theirs],
            later_conflicts: vec![55],
            warning: None,
        },
    );

    let out = board(
        &home,
        &repo,
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "src/a.rs", "--task", "acme-43"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[1].starts_with(
            "! overlaps later lease #55 github.com/acme/web \"src/**\" for acme-54 (doing) by t3:9999 expires in "
        ),
        "{}",
        lines[1]
    );
    assert!(
        lines[1].ends_with(" -> your earlier lease holds; note acme-54 and go on"),
        "{}",
        lines[1]
    );
}

#[test]
fn lease_dir_leases_in_that_repo_and_errors_with_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = home_for(dir.path(), &stub.url);
    let repo_a = git_repo(dir.path(), "git@github.com:acme/web.git");

    let other_dir = tempfile::tempdir().unwrap();
    let repo_b = git_repo(other_dir.path(), "git@github.com:acme/skills.git");

    let no_origin_dir = tempfile::tempdir().unwrap();
    let no_origin = no_origin_dir.path().join("local-only");
    std::fs::create_dir_all(&no_origin).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&no_origin)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap()
            .success()
    );
    let not_a_repo = tempfile::tempdir().unwrap();
    let file_path = not_a_repo.path().join("file.txt");
    std::fs::write(&file_path, "x").unwrap();

    stub.on(
        &format!("GET /v1/agents/{}", enc(ME)),
        agent_fixture(ME, DEVICE, AgentState::Running),
    );
    let mut skills_lease = lease(91, ME, Some("acme-8"), "scripts/**");
    skills_lease.repo = "github.com/acme/skills".into();
    stub.on(
        "POST /v1/leases",
        LeaseResult {
            leases: vec![skills_lease],
            conflicts: vec![],
            later_conflicts: vec![],
            warning: None,
        },
    );

    // Valid --dir in another checkout leases in that checkout's repo.
    let repo_b_str = repo_b.to_string_lossy().into_owned();
    let out = board(
        &home,
        &repo_a,
        &[("T3CODE_THREAD_ID", "thread-1")],
        &[
            "lease",
            "scripts/**",
            "--task",
            "acme-8",
            "--dir",
            &repo_b_str,
        ],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stub.requests_to("POST /v1/leases")[0].body["repo"],
        "github.com/acme/skills"
    );
    stub.clear();

    // Nonexistent directory errors with a next step and sends no lease request.
    stub.clear();
    let missing = dir.path().join("does-not-exist");
    let missing_str = missing.to_string_lossy().into_owned();
    let out = board(
        &home,
        &repo_a,
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "scripts/**", "--dir", &missing_str],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("does not exist") && err.contains("--dir"),
        "{err}"
    );
    assert!(stub.requests_to("POST /v1/leases").is_empty());

    // Regular file (not a directory) and a path traversing a file both report not a directory.
    let file_str = file_path.to_string_lossy().into_owned();
    for bad in [file_str.clone(), format!("{file_str}/sub")] {
        let out = board(
            &home,
            &repo_a,
            &[("T3CODE_THREAD_ID", "thread-1")],
            &["lease", "scripts/**", "--dir", &bad],
            "",
        );
        assert!(!out.status.success());
        let err = stderr(&out);
        assert!(
            err.contains("not a directory") && err.contains("--dir"),
            "{bad}: {err}"
        );
    }

    // Leading `~` in --dir expands against HOME (important for MCP callers without a shell).
    stub.clear();
    let other_home = other_dir.path().to_string_lossy().into_owned();
    let out = board(
        &home,
        &repo_a,
        &[("T3CODE_THREAD_ID", "thread-1"), ("HOME", &other_home)],
        &["lease", "scripts/**", "--task", "acme-8", "--dir", "~/repo"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stub.requests_to("POST /v1/leases")[0].body["repo"],
        "github.com/acme/skills"
    );
    stub.clear();

    // Directory outside any git repo errors with a next step.
    let not_repo_str = not_a_repo.path().to_string_lossy().into_owned();
    let out = board(
        &home,
        &repo_a,
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "scripts/**", "--dir", &not_repo_str],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("not in a git repo") && err.contains("--dir"),
        "{err}"
    );

    // Origin-less git repo passed explicitly in --dir errors with a next step even with --task.
    let no_origin_str = no_origin.to_string_lossy().into_owned();
    let out = board(
        &home,
        &repo_a,
        &[("T3CODE_THREAD_ID", "thread-1")],
        &[
            "lease",
            "scripts/**",
            "--task",
            "acme-8",
            "--dir",
            &no_origin_str,
        ],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("no origin remote") && err.contains("--dir"),
        "{err}"
    );
    assert!(stub.requests_to("POST /v1/leases").is_empty());
}

#[test]
fn lease_without_origin_uses_task_repo_or_single_board_repo() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    let home = home_for(dir.path(), &stub.url);
    let scratch = tempfile::tempdir().unwrap();

    stub.on(
        &format!("GET /v1/agents/{}", enc(ME)),
        agent_fixture(ME, DEVICE, AgentState::Running),
    );
    stub.on(
        "GET /v1/boards/acme",
        board_fixture("acme", &["github.com/acme/web"]),
    );
    stub.on(
        "GET /v1/tasks/acme-8",
        TaskDetail {
            task: task_fixture("acme-8", "Edit web"),
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    stub.on(
        "GET /v1/boards/multi",
        board_fixture("multi", &["github.com/acme/web", "github.com/acme/skills"]),
    );
    let mut routed = task_fixture("multi-2", "Edit skills");
    routed.repo = Some("github.com/acme/skills".into());
    stub.on(
        "GET /v1/tasks/multi-2",
        TaskDetail {
            task: routed,
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    stub.on(
        "GET /v1/tasks/multi-3",
        TaskDetail {
            task: task_fixture("multi-3", "Ambiguous multi"),
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    stub.on("GET /v1/boards/empty", board_fixture("empty", &[]));
    stub.on(
        "GET /v1/tasks/empty-1",
        TaskDetail {
            task: task_fixture("empty-1", "No repo"),
            notes: vec![],
            evidence: vec![],
            blocks: vec![],
            children: vec![],
            child_tasks: vec![],
            history: vec![],
            handles: Default::default(),
        },
    );
    stub.on(
        "POST /v1/leases",
        LeaseResult {
            leases: vec![lease(92, ME, Some("acme-8"), "src/**")],
            conflicts: vec![],
            later_conflicts: vec![],
            warning: None,
        },
    );

    // Outside any repo, --task on a single-repo board resolves that board's repo.
    let out = board(
        &home,
        scratch.path(),
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "src/**", "--task", "acme-8"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stub.requests_to("POST /v1/leases")[0].body["repo"],
        "github.com/acme/web"
    );

    // On a multi-repo board, task.repo overrides board.repos.
    stub.clear();
    let out = board(
        &home,
        scratch.path(),
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "scripts/**", "--task", "multi-2"],
        "",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stub.requests_to("POST /v1/leases")[0].body["repo"],
        "github.com/acme/skills"
    );
    stub.clear();

    // Multi-repo board with no task.repo errors naming the repos and --dir.
    let out = board(
        &home,
        scratch.path(),
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "scripts/**", "--task", "multi-3"],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("covers several repos")
            && err.contains("github.com/acme/skills")
            && err.contains("--dir"),
        "{err}"
    );

    // Board with no repos errors naming --dir.
    let out = board(
        &home,
        scratch.path(),
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "scripts/**", "--task", "empty-1"],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("lists no repo") && err.contains("--dir"),
        "{err}"
    );

    // No --task and no --dir outside a repo errors naming --dir and task.
    let out = board(
        &home,
        scratch.path(),
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "src/**"],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("--dir") && err.contains("task"), "{err}");

    // A non-NotFound error reading the task propagates instead of silently falling back to board.repos.
    stub.on_error(
        "GET /v1/tasks/acme-9",
        500,
        "internal",
        "database busy",
        "try again",
    );
    stub.clear();
    let out = board(
        &home,
        scratch.path(),
        &[("T3CODE_THREAD_ID", "thread-1")],
        &["lease", "src/**", "--task", "acme-9"],
        "",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("database busy"), "{err}");
    assert!(stub.requests_to("POST /v1/leases").is_empty());
}
