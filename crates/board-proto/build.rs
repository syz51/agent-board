//! Records the git commit the crate is built from as `BOARD_COMMIT`: the `BOARD_COMMIT`
//! environment variable when set (as the Docker build does, without `.git`), else
//! `git rev-parse --short=12 HEAD`, else `unknown`.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-env-changed=BOARD_COMMIT");
    let commit = match std::env::var("BOARD_COMMIT") {
        Ok(commit) if !commit.trim().is_empty() => commit.trim().to_owned(),
        _ => git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_owned()),
    };
    // Build again when HEAD moves: HEAD itself, the branch it points at, and packed refs.
    let mut watched = vec!["build.rs".to_owned()];
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    for name in ["HEAD", "packed-refs"].into_iter().chain(branch.as_deref()) {
        if let Some(path) = git(&["rev-parse", "--git-path", name])
            && Path::new(&path).exists()
        {
            watched.push(path);
        }
    }
    for path in watched {
        println!("cargo:rerun-if-changed={path}");
    }
    // It goes into a response header: keep it to a plain token.
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    let commit = if commit.len() <= 64 && commit.chars().all(plain) {
        commit
    } else {
        "unknown".to_owned()
    };
    println!("cargo:rustc-env=BOARD_COMMIT={commit}");
}
