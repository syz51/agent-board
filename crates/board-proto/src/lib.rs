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

/// The git commit this build was made from (12 hex digits), or `unknown`.
pub const COMMIT: &str = env!("BOARD_COMMIT");

/// `VERSION (COMMIT)`, as `board --version` prints it.
pub const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("BOARD_COMMIT"), ")");

/// Response header with the server's [`VERSION`], on every answer.
pub const VERSION_HEADER: &str = "x-board-version";

/// Response header with the server's [`COMMIT`], on every answer.
pub const COMMIT_HEADER: &str = "x-board-commit";
