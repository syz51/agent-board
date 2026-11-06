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
pub mod render;
pub mod state;
pub mod t3;
