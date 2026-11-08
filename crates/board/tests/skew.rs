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

fn this_stamp() -> u64 {
    board::BUILT
        .parse()
        .expect("the tests build in a git checkout, so the build has an order stamp")
}

#[test]
fn an_older_client_says_calls_still_work_and_what_picks_up_the_new_build() {
    let later = (this_stamp() + 60).to_string();
    let err = ls_against(|stub| stub.build_stamped(board_proto::VERSION, "0123456789ab", &later));
    // The test binary is the installed board, so this device is not upgraded yet.
    assert_eq!(
        err.trim(),
        format!(
            "board: warning: this session's board, {v} ({c}), is older than the board server, \
             {v} (0123456789ab); calls still work, so keep working. This device is not upgraded \
             yet: the device upgrade (deploy/upgrade.sh, run by whoever deploys) installs the new \
             build; nothing is needed from you.",
            v = board_proto::VERSION,
            c = board::COMMIT,
        )
    );
}

#[test]
fn an_older_server_says_to_upgrade_the_server() {
    let earlier = (this_stamp() - 60).to_string();
    let err = ls_against(|stub| {
        stub.build_stamped(board_proto::VERSION, "0123456789ab", &earlier);
    });
    assert_eq!(
        err.trim(),
        format!(
            "board: warning: the board server, {v} (0123456789ab), is older than this session's \
             board, {v} ({c}); calls still work, so keep working. Whoever deploys should upgrade \
             the server (docs/DEPLOY.md).",
            v = board_proto::VERSION,
            c = board::COMMIT,
        )
    );
}

#[test]
fn a_server_without_a_stamp_gets_the_unordered_warning() {
    let err = ls_against(|stub| stub.build(board_proto::VERSION, "0123456789ab"));
    assert!(
        err.contains("upgrade the older one (docs/DEPLOY.md)"),
        "{err}"
    );
    // The same build with its stamp: no warning.
    let err = ls_against(|stub| {
        stub.build_stamped(board_proto::VERSION, board::COMMIT, board::BUILT);
    });
    assert_eq!(err, "");
}
