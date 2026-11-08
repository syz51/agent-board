//! The version-skew warning names the older side by the builds' order stamps.

mod common;

use common::*;

/// Runs `board ls` against a stub server on `build`, and returns its stderr.
fn ls_against(build: impl FnOnce(&Stub)) -> String {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    stub.on("GET /v1/tasks", Vec::<board_proto::Task>::new());
    build(&stub);
    let home = home_for(dir.path(), &stub.url);
    let out = board(&home, dir.path(), &[], &["ls"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    stderr(&out)
}

/// This build's order stamp; `None` for a build outside a git checkout, which has no stamp
/// or commit to compare, so these tests have nothing to check there.
fn this_stamp() -> Option<u64> {
    let stamp = board::BUILT.parse().ok();
    if stamp.is_none() || board::COMMIT == "unknown" {
        eprintln!("skipped: this build has no order stamp or commit");
        return None;
    }
    stamp
}

const SERVER: &str = "0123456789ab";

#[test]
fn an_older_client_says_calls_still_work_and_what_picks_up_the_new_build() {
    let Some(stamp) = this_stamp() else { return };
    let later = (stamp + 60).to_string();
    let err = ls_against(|stub| stub.build_stamped(board_proto::VERSION, SERVER, Some(&later)));
    // A CLI command runs the installed board, so this device is not upgraded yet.
    assert_eq!(
        err.trim(),
        format!(
            "board: warning: this session's board, {v} ({c}), is older than the board server, \
             {v} ({SERVER}); calls still work, so keep working. This device is not upgraded \
             yet: the device upgrade (deploy/upgrade.sh, run by whoever deploys) installs the new \
             build; nothing is needed from you.",
            v = board_proto::VERSION,
            c = board::COMMIT,
        )
    );
}

#[test]
fn an_older_server_says_to_upgrade_the_server() {
    let Some(stamp) = this_stamp() else { return };
    let earlier = (stamp - 60).to_string();
    let expected = format!(
        "board: warning: the board server, {v} ({SERVER}), is older than this session's board, \
         {v} ({c}): whoever deploys should upgrade the server (docs/DEPLOY.md). Until then, \
         calls new in this build may fail.",
        v = board_proto::VERSION,
        c = board::COMMIT,
    );
    let err = ls_against(|stub| {
        stub.build_stamped(board_proto::VERSION, SERVER, Some(&earlier));
    });
    assert_eq!(err.trim(), expected);
    // A server that sends no stamp predates stamps, so it is older too.
    let err = ls_against(|stub| stub.build_stamped(board_proto::VERSION, SERVER, None));
    assert_eq!(err.trim(), expected);
}

#[test]
fn a_server_built_without_a_stamp_gets_the_unordered_warning() {
    if this_stamp().is_none() {
        return;
    }
    let err = ls_against(|stub| stub.build(board_proto::VERSION, SERVER));
    assert!(
        err.contains("upgrade the older one (docs/DEPLOY.md)"),
        "{err}"
    );
    // The same build with its stamp: no warning.
    let err = ls_against(|stub| {
        stub.build_stamped(board_proto::VERSION, board::COMMIT, Some(board::BUILT));
    });
    assert_eq!(err, "");
}
