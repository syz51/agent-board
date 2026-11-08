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
        "! overlaps lease #86 github.com/acme/web \"src/a.rs\" for acme-7 (review) by t3:4600 expires in ",
        "! overlaps lease #87 github.com/acme/web \"src/b.rs\" for acme-9 by t3:gone expires in ",
        "! overlaps lease #88 github.com/acme/web \"src/c.rs\" for no task by t3:4600 expires in ",
    ];
    for (line, want) in lines[1..].iter().zip(expected) {
        assert!(line.starts_with(want), "{line}");
        assert!(line.ends_with(" -> coordinate before editing"), "{line}");
    }
    // One lookup per distinct task and holder.
    assert_eq!(stub.requests_to("GET /v1/tasks/acme-7").len(), 1);
    assert_eq!(
        stub.requests_to(&format!("GET /v1/agents/{}", enc(PEER)))
            .len(),
        1
    );
}
