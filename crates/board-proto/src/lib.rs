//! Wire types shared by the agent-board server and its clients.
//!
//! Every request and response body of the HTTP API (`docs/API.md`) is a type in this crate,
//! serialized as JSON with `snake_case` field names and enum values. Identifier types
//! validate their format when deserialized, so a value of one of them is well formed.

mod api;
mod ids;
mod limits;
mod model;
mod repo;

pub use api::*;
pub use ids::*;
pub use limits::*;
pub use model::*;
pub use repo::{REPO_KEY_MAX, normalize_repo};

/// Version prefix of every API path.
pub const API_PREFIX: &str = "/v1";

/// Header carrying the calling agent's id on agent requests.
pub const AGENT_HEADER: &str = "x-board-agent";

/// The version of this build: the workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Response header with the server's [`VERSION`], on every answer.
pub const VERSION_HEADER: &str = "x-board-version";

/// Response header with the commit the server was built from (12 hex digits or `unknown`),
/// on every answer.
pub const COMMIT_HEADER: &str = "x-board-commit";

/// Response header with the server build's order stamp, on every answer: its commit's
/// committer time in seconds since the Unix epoch, or `unknown`. A newer build has a higher
/// stamp. Servers before it do not send it.
pub const BUILT_HEADER: &str = "x-board-built";

/// Response header advertising the server's supported protocol and capability features
/// (comma-separated tokens), on every answer.
pub const FEATURES_HEADER: &str = "x-board-features";

/// Protocol/capability feature tokens advertised by this build of the board server.
pub const SERVER_FEATURES: &str = "v1-json,sse-v1,device-caps-v1,usage-buckets-v1,runs-v1";

/// Legacy `/v1/usage` and `/v1/usage/reserve` alias this provider in `usage_buckets`.
pub const LEGACY_USAGE_PROVIDER: &str = "anthropic";

/// Legacy `/v1/usage` and `/v1/usage/reserve` alias this account in `usage_buckets`.
pub const LEGACY_USAGE_ACCOUNT: &str = "default";

/// Default seconds without a device heartbeat before a device counts as `stale`.
pub const DEVICE_STALE_SECS_DEFAULT: u64 = 90;

/// Default lease TTL in seconds for managed runs before the sweeper marks them `orphaned`.
pub const RUN_LEASE_TTL_SECS_DEFAULT: u64 = 180;

/// Runs `GET /v1/runs` lists when the request names no `limit`.
pub const RUNS_LIMIT_DEFAULT: u32 = 100;

/// Most runs one `GET /v1/runs` answer lists.
pub const RUNS_LIMIT_MAX: u32 = 500;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_header_names_stay_fixed_within_v1() {
        // Clients of every build read these names: renaming one breaks the skew warning.
        assert_eq!(VERSION_HEADER, "x-board-version");
        assert_eq!(COMMIT_HEADER, "x-board-commit");
        assert_eq!(BUILT_HEADER, "x-board-built");
        assert_eq!(FEATURES_HEADER, "x-board-features");
        // Lowercase, as `HeaderName::from_static` requires.
        for name in [
            AGENT_HEADER,
            VERSION_HEADER,
            COMMIT_HEADER,
            BUILT_HEADER,
            FEATURES_HEADER,
        ] {
            assert_eq!(name, name.to_ascii_lowercase());
        }
    }
}
