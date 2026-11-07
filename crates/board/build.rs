//! Records the commit this binary is built from as `BOARD_COMMIT` (see `build_commit.rs`).
//! Only this leaf crate carries it, so a new commit rebuilds `board` alone.

use std::path::{Path, PathBuf};
use std::process::Command;

include!("build_commit.rs");

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-env-changed=BOARD_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_commit.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    // The workspace root; git answers only when it is the top of its own repository, so
    // sources unpacked inside another repository do not take that repository's commit.
    let root = manifest
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| manifest.clone());
    let own_repo = git(&root, &["rev-parse", "--show-toplevel"])
        .and_then(|top| Path::new(&top).canonicalize().ok())
        .is_some_and(|top| top == root);
    let from_git = own_repo
        .then(|| git(&root, &["rev-parse", "HEAD"]))
        .flatten();
    let commit = normalize_commit(
        std::env::var("BOARD_COMMIT").ok().as_deref(),
        from_git.as_deref(),
    );
    if own_repo {
        // Build again when HEAD moves. `logs/HEAD` records every move (commits, checkouts,
        // resets), also when the branch ref is packed; HEAD and the loose ref cover a
        // repository without reflogs. Only paths that exist, or cargo reruns every build.
        let branch = git(&root, &["symbolic-ref", "-q", "HEAD"]);
        for name in ["HEAD", "logs/HEAD", "packed-refs"]
            .into_iter()
            .chain(branch.as_deref())
        {
            if let Some(path) = git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-path", name],
            ) && Path::new(&path).exists()
            {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    println!("cargo:rustc-env=BOARD_COMMIT={commit}");
}
