//! Wire types shared by the agent-board server and its clients.
//!
//! Every request and response body of the HTTP API (`docs/API.md`) is a type in this crate,
//! serialized as JSON with `snake_case` field names and enum values. Identifier types
//! validate their format when deserialized, so a value of one of them is well formed.

mod api;
mod health;
mod ids;
mod limits;
mod model;
mod repo;

pub use api::*;
pub use health::*;
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
pub const SERVER_FEATURES: &str = "v1-json,sse-v1,device-caps-v1,usage-buckets-v1,runs-v1,dispatch-claim-v1,scheduler-telemetry-v1,run-events-v1,scheduler-autotune-v1,device-rpc-v1,health-schedules-v1";

/// Legacy `/v1/usage` and `/v1/usage/reserve` alias this provider in `usage_buckets`.
pub const LEGACY_USAGE_PROVIDER: &str = "anthropic";

/// Legacy `/v1/usage` and `/v1/usage/reserve` alias this account in `usage_buckets`.
pub const LEGACY_USAGE_ACCOUNT: &str = "default";

/// Default seconds without a device heartbeat before a device counts as `stale`.
pub const DEVICE_STALE_SECS_DEFAULT: u64 = 90;

/// Default lease TTL in seconds for managed runs before the sweeper marks them `orphaned`.
pub const RUN_LEASE_TTL_SECS_DEFAULT: u64 = 180;

/// Default grace period in seconds before pending sticky wakes and `waiting_external` runs
/// on a stale device fall back to the global pool (`docs/SCHEDULER_DESIGN.md`).
pub const STICKY_GRACE_SECS_DEFAULT: u64 = 600;

/// Default number of runs claimed by `POST /v1/dispatch/claim` when `max_claims` is omitted.
pub const DISPATCH_MAX_CLAIMS_DEFAULT: u32 = 1;

/// Maximum number of runs one `POST /v1/dispatch/claim` call may claim at once.
pub const DISPATCH_MAX_CLAIMS_LIMIT: u32 = 4;

/// Default version tag recorded on runs leased by the built-in scheduler policy.
pub const SCHEDULER_POLICY_VERSION_DEFAULT: &str = "v1-default";

/// Default number of evaluations returned by `GET /v1/scheduler/evaluations`.
pub const SCHEDULER_EVALUATIONS_LIMIT_DEFAULT: u32 = 20;

/// Maximum number of evaluations returned by `GET /v1/scheduler/evaluations`.
pub const SCHEDULER_EVALUATIONS_LIMIT_MAX: u32 = 200;

/// Runs `GET /v1/runs` lists when the request names no `limit`.
pub const RUNS_LIMIT_DEFAULT: u32 = 100;

/// Most runs one `GET /v1/runs` answer lists.
pub const RUNS_LIMIT_MAX: u32 = 500;

/// Maximum bounded live run events retained per run in `run_events` (`docs/SCHEDULER_DESIGN.md`,
/// Section 5.2).
pub const RUN_EVENTS_CAP: usize = 200;

/// Maximum events accepted in a single `POST /v1/runs/{id}/events` batch.
pub const RUN_EVENTS_BATCH_MAX: usize = 200;

/// Default and maximum events returned by a JSON `GET /v1/runs/{id}/events` query.
pub const RUN_EVENTS_LIMIT_DEFAULT: u32 = 200;
pub const RUN_EVENTS_LIMIT_MAX: u32 = 200;

/// Maximum length in characters of a live run event's `summary`.
pub const RUN_EVENT_SUMMARY_CHARS_MAX: usize = 300;

/// Maximum serialized byte size of a live run event's optional `meta` JSON value.
pub const RUN_EVENT_META_BYTES_MAX: usize = 16_384;

/// Maximum length in characters of `SteerRunRequest::message`.
pub const RUN_STEER_MESSAGE_CHARS_MAX: usize = 4_000;

/// Maximum length in characters of `CancelRunRequest::reason`.
pub const RUN_CANCEL_REASON_CHARS_MAX: usize = 500;

/// Maximum length in characters of a device RPC method name.
pub const DEVICE_RPC_METHOD_CHARS_MAX: usize = 64;

/// Maximum serialized byte size of a device RPC request `params` payload.
pub const DEVICE_RPC_PARAMS_BYTES_MAX: usize = 65_536;

/// Maximum serialized byte size of a device RPC reply `result` payload.
pub const DEVICE_RPC_RESULT_BYTES_MAX: usize = 262_144;

/// Maximum length in characters of a device RPC reply `error` message.
pub const DEVICE_RPC_ERROR_CHARS_MAX: usize = 1_000;

/// Default timeout in seconds for `POST /v1/devices/{name}/rpc`.
pub const DEVICE_RPC_TIMEOUT_SECS_DEFAULT: u64 = 30;

/// Maximum timeout in seconds for `POST /v1/devices/{name}/rpc`.
pub const DEVICE_RPC_TIMEOUT_SECS_MAX: u64 = 60;

/// Default long-poll wait in seconds for `GET /v1/devices/{name}/rpc/poll`.
pub const DEVICE_RPC_POLL_WAIT_SECS_DEFAULT: u64 = 25;

/// Maximum long-poll wait in seconds for `GET /v1/devices/{name}/rpc/poll`.
pub const DEVICE_RPC_POLL_WAIT_SECS_MAX: u64 = 30;

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

    #[test]
    fn v2_backlog_fields_are_v1_backward_compatible_and_open_enums_decode_unknown() {
        // Legacy v1 Task JSON without any v2 fields deserializes cleanly with defaults.
        let legacy_task = serde_json::json!({
            "id": "factory-1",
            "board": "factory",
            "parent": null,
            "title": "Legacy task",
            "body": "",
            "acceptance": "",
            "status": "open",
            "priority": "p2",
            "blocked_reason": null,
            "owner": null,
            "owner_stale": false,
            "blockers": [],
            "ready": true,
            "auto": true,
            "created_by": "t3:a",
            "version": 1,
            "created_at": "2026-11-06T12:00:00Z",
            "updated_at": "2026-11-06T12:00:00Z"
        });
        let task: Task = serde_json::from_value(legacy_task).unwrap();
        assert_eq!(task.origin, None);
        assert_eq!(task.size, None);
        assert_eq!(task.cooled_at, None);
        assert_eq!(task.ready_at, None);
        assert_eq!(task.report_count, 0);
        assert_eq!(task.effective_priority, None);

        // Legacy v1 Board JSON without `kind` or `curate_every_secs` defaults to Project / None.
        let legacy_board = serde_json::json!({
            "slug": "factory",
            "title": "Factory",
            "goal": "",
            "status": "active",
            "repos": [],
            "autopilot": false,
            "coordinator": null,
            "home_devices": [],
            "counts": { "open": 0, "doing": 0, "blocked": 0, "review": 0, "done": 0, "dropped": 0, "ready": 0 },
            "open_asks": 0,
            "created_at": "2026-11-06T12:00:00Z",
            "updated_at": "2026-11-06T12:00:00Z"
        });
        let board: Board = serde_json::from_value(legacy_board).unwrap();
        assert_eq!(board.kind, BoardKind::Project);
        assert_eq!(board.curate_every_secs, None);

        // Open enums decode unknown values cleanly and TaskSize weights match WSJF spec.
        assert_eq!(
            serde_json::from_str::<BoardKind>("\"future_kind\"").unwrap(),
            BoardKind::Unknown
        );
        assert_eq!(
            serde_json::from_str::<TaskOrigin>("\"future_origin\"").unwrap(),
            TaskOrigin::Unknown
        );
        assert_eq!(
            serde_json::from_str::<TaskSize>("\"xxl\"").unwrap(),
            TaskSize::Unknown
        );
        assert_eq!(
            serde_json::from_str::<RunKind>("\"curate\"").unwrap(),
            RunKind::Curate
        );
        assert!(RunKind::Curate.grants_coordinator_rights());
        assert_eq!(
            serde_json::from_str::<DispatchHoldReason>("\"lease_overlap\"").unwrap(),
            DispatchHoldReason::LeaseOverlap
        );
        assert_eq!(
            serde_json::from_str::<DispatchHoldReason>("\"spawn_backoff\"").unwrap(),
            DispatchHoldReason::SpawnBackoff
        );
        assert_eq!(TaskSize::Xs.weight(), 1.0);
        assert_eq!(TaskSize::S.weight(), 1.5);
        assert_eq!(TaskSize::M.weight(), 3.0);
        assert_eq!(TaskSize::L.weight(), 6.0);
        assert_eq!(SchedulerWeights::default().w_age, W_AGE_MAX);
    }
}
