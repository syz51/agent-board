//! agent-board client: CLI, MCP server, hooks, device daemon and configuration.
//!
//! The `board` binary is a thin wrapper around [`cli::run`]; the modules are a library so
//! integration tests can drive the daemon with a fake `t3`.

pub mod cli;
pub mod client;
pub mod config;
pub mod daemon;
pub mod hook;
pub mod identity;
pub mod install;
pub mod mcp;
pub mod ops;
pub mod relay;
pub mod render;
pub mod state;
pub mod t3;
pub mod worktree;

/// The git commit this binary was built from (12 hex digits), or `unknown`.
pub const COMMIT: &str = env!("BOARD_COMMIT");

/// `VERSION (COMMIT)`, as `board --version` prints it.
pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("BOARD_COMMIT"), ")");

#[cfg(test)]
mod build_commit {
    include!("../build_commit.rs");

    #[test]
    fn commits_are_twelve_lowercase_hex_digits_or_unknown() {
        let sha = "0123456789ABCDEF0123456789abcdef01234567";
        assert_eq!(normalize_commit(None, Some(sha)), "0123456789ab");
        // The override wins over git, trimmed and cut to 12 digits.
        assert_eq!(
            normalize_commit(Some(" FEDCBA9876543210 \n"), Some(sha)),
            "fedcba987654"
        );
        // An empty override falls back to git.
        assert_eq!(normalize_commit(Some("  "), Some(sha)), "0123456789ab");
        // Not a commit: a tag, a short hash, header-unsafe text, nothing at all.
        for bad in [
            "v0.1.0",
            "0123456",
            "0123456789ab\r\nX-Evil: 1",
            "0123456789xyz0",
        ] {
            assert_eq!(normalize_commit(Some(bad), Some(sha)), "unknown", "{bad:?}");
        }
        assert_eq!(normalize_commit(None, None), "unknown");
    }
}
