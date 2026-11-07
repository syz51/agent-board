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
