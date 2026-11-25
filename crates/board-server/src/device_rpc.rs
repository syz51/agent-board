//! In-memory RPC broker bridging user-only `POST /v1/devices/{name}/rpc` requests to
//! device-only `GET /v1/devices/{name}/rpc/poll` and `POST /v1/devices/{name}/rpc/{id}/reply`.
//!
//! Nothing is persisted in SQLite: RPC calls are short-lived interactive requests routed
//! directly to awake `board watch` daemons. Cancellation safety is guaranteed by a drop guard
//! that removes timed-out or disconnected calls from both the pending queue and the waiter map.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use board_proto::{
    DEVICE_RPC_ERROR_CHARS_MAX, DEVICE_RPC_METHOD_CHARS_MAX, DEVICE_RPC_PARAMS_BYTES_MAX,
    DEVICE_RPC_POLL_WAIT_SECS_DEFAULT, DEVICE_RPC_POLL_WAIT_SECS_MAX, DEVICE_RPC_RESULT_BYTES_MAX,
    DEVICE_RPC_TIMEOUT_SECS_DEFAULT, DEVICE_RPC_TIMEOUT_SECS_MAX, DeviceName, DeviceRpcCall,
    DeviceRpcReply, DeviceRpcRequest, DeviceRpcResponse,
};
use serde_json::Value;
use tokio::sync::{Notify, oneshot};

use crate::auth::random_hex;
use crate::error::{Error, Result, invalid, not_found, refused};
use crate::ops::devices::check_token;

/// Maximum unpolled RPC calls queued for one device.
const MAX_PENDING_PER_DEVICE: usize = 32;
/// Maximum total in-flight (pending + executing) RPC calls for one device.
const MAX_INFLIGHT_PER_DEVICE: usize = 64;
/// Maximum queued calls returned by a single poll response.
const MAX_POLL_BATCH: usize = 8;

/// Validated parameters for one device RPC call.
pub(crate) struct ValidatedRpcRequest {
    pub method: String,
    pub params: Value,
    pub timeout_secs: u64,
}

/// Validates a `POST /v1/devices/{name}/rpc` body.
pub(crate) fn validate_request(req: DeviceRpcRequest) -> Result<ValidatedRpcRequest> {
    let method = check_token("method", &req.method, DEVICE_RPC_METHOD_CHARS_MAX)?;
    let timeout_secs = req.timeout_secs.unwrap_or(DEVICE_RPC_TIMEOUT_SECS_DEFAULT);
    if !(1..=DEVICE_RPC_TIMEOUT_SECS_MAX).contains(&timeout_secs) {
        return Err(invalid(
            format!("timeout_secs={timeout_secs} is out of range"),
            format!("send timeout_secs between 1 and {DEVICE_RPC_TIMEOUT_SECS_MAX}"),
        ));
    }
    let bytes = serde_json::to_vec(&req.params).map_err(|e| {
        invalid(
            format!("params failed to serialize: {e}"),
            "send valid JSON",
        )
    })?;
    if bytes.len() > DEVICE_RPC_PARAMS_BYTES_MAX {
        return Err(invalid(
            format!(
                "params is {} bytes; the limit is {DEVICE_RPC_PARAMS_BYTES_MAX}",
                bytes.len()
            ),
            format!("keep params under {DEVICE_RPC_PARAMS_BYTES_MAX} bytes"),
        ));
    }
    Ok(ValidatedRpcRequest {
        method,
        params: req.params,
        timeout_secs,
    })
}

/// Validates the `wait_secs` query parameter of `GET /v1/devices/{name}/rpc/poll`.
pub(crate) fn validate_poll_wait(wait_secs: Option<u64>) -> Result<u64> {
    let secs = wait_secs.unwrap_or(DEVICE_RPC_POLL_WAIT_SECS_DEFAULT);
    if secs > DEVICE_RPC_POLL_WAIT_SECS_MAX {
        return Err(invalid(
            format!("wait_secs={secs} is out of range"),
            format!("send wait_secs between 0 and {DEVICE_RPC_POLL_WAIT_SECS_MAX}"),
        ));
    }
    Ok(secs)
}

/// Validates a `POST /v1/devices/{name}/rpc/{id}/reply` body.
pub(crate) fn validate_reply(mut reply: DeviceRpcReply) -> Result<DeviceRpcReply> {
    if let Some(result) = &reply.result {
        let bytes = serde_json::to_vec(result).map_err(|e| {
            invalid(
                format!("result failed to serialize: {e}"),
                "send valid JSON",
            )
        })?;
        if bytes.len() > DEVICE_RPC_RESULT_BYTES_MAX {
            return Err(invalid(
                format!(
                    "result is {} bytes; the limit is {DEVICE_RPC_RESULT_BYTES_MAX}",
                    bytes.len()
                ),
                format!("keep result under {DEVICE_RPC_RESULT_BYTES_MAX} bytes"),
            ));
        }
    }
    if reply.ok {
        if reply.error.is_some() {
            return Err(invalid(
                "ok=true reply must not carry an error message",
                "omit error when ok=true, or set ok=false",
            ));
        }
    } else {
        let msg = reply
            .error
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                invalid(
                    "ok=false reply requires a non-empty error message",
                    "set error to explain why the RPC failed",
                )
            })?;
        if msg.chars().count() > DEVICE_RPC_ERROR_CHARS_MAX {
            return Err(invalid(
                format!("error is too long (limit {DEVICE_RPC_ERROR_CHARS_MAX} characters)"),
                format!("shorten error to at most {DEVICE_RPC_ERROR_CHARS_MAX} characters"),
            ));
        }
        reply.error = Some(msg.to_owned());
    }
    Ok(reply)
}

#[derive(Default)]
struct DeviceQueue {
    pending: VecDeque<DeviceRpcCall>,
    waiters: HashMap<String, oneshot::Sender<DeviceRpcReply>>,
    notify: Arc<Notify>,
}

/// In-memory RPC broker shared by all handlers on [`crate::state::AppState`].
#[derive(Clone, Default)]
pub(crate) struct DeviceRpcBroker {
    queues: Arc<Mutex<HashMap<DeviceName, DeviceQueue>>>,
}

struct CallCleanup {
    queues: Arc<Mutex<HashMap<DeviceName, DeviceQueue>>>,
    device: DeviceName,
    id: String,
    active: bool,
}

impl CallCleanup {
    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for CallCleanup {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut map = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(q) = map.get_mut(&self.device) {
            q.pending.retain(|c| c.id != self.id);
            q.waiters.remove(&self.id);
            if q.pending.is_empty() && q.waiters.is_empty() && Arc::strong_count(&q.notify) == 1 {
                map.remove(&self.device);
            }
        }
    }
}

impl DeviceRpcBroker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Enqueues an RPC call for `device` and waits for its reply up to `req.timeout_secs`,
    /// or until `stopped` resolves.
    pub(crate) async fn call(
        &self,
        device: &DeviceName,
        req: ValidatedRpcRequest,
        stopped: impl Future<Output = ()>,
    ) -> Result<DeviceRpcResponse> {
        let id = format!("rpc-{}", random_hex::<12>());
        let method = req.method;
        let timeout_secs = req.timeout_secs;
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
            let queue = map.entry(device.clone()).or_default();
            if queue.pending.len() >= MAX_PENDING_PER_DEVICE
                || queue.waiters.len() >= MAX_INFLIGHT_PER_DEVICE
            {
                return Err(refused(
                    format!("device {device} RPC queue is full"),
                    "wait for in-flight device RPC calls to finish and retry",
                ));
            }
            queue.pending.push_back(DeviceRpcCall {
                id: id.clone(),
                method: method.clone(),
                params: req.params,
                timeout_secs,
            });
            queue.waiters.insert(id.clone(), tx);
            queue.notify.notify_one();
        }

        let mut cleanup = CallCleanup {
            queues: Arc::clone(&self.queues),
            device: device.clone(),
            id: id.clone(),
            active: true,
        };

        let sleep = tokio::time::sleep(Duration::from_secs(timeout_secs));
        tokio::pin!(sleep);
        tokio::pin!(stopped);

        tokio::select! {
            res = rx => {
                cleanup.disarm();
                match res {
                    Ok(reply) => Ok(DeviceRpcResponse {
                        id,
                        ok: reply.ok,
                        result: reply.result,
                        error: reply.error,
                    }),
                    Err(_) => Err(Error::Internal("device RPC waiter channel closed".into())),
                }
            }
            () = &mut sleep => {
                drop(cleanup);
                Ok(DeviceRpcResponse {
                    id,
                    ok: false,
                    result: None,
                    error: Some(format!(
                        "device {device} did not answer rpc {method} within {timeout_secs}s \
                         (device may be asleep or board watch offline)"
                    )),
                })
            }
            () = &mut stopped => {
                drop(cleanup);
                Err(Error::Internal("board server is shutting down".into()))
            }
        }
    }

    /// Long-polls for queued RPC calls addressed to `device`, waiting at most `wait_secs`
    /// (or until `stopped` resolves). Returns an empty vector when idle or stopping.
    pub(crate) async fn poll(
        &self,
        device: &DeviceName,
        wait_secs: u64,
        stopped: impl Future<Output = ()>,
    ) -> Vec<DeviceRpcCall> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        tokio::pin!(stopped);

        loop {
            let notify = {
                let mut map = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
                let queue = map.entry(device.clone()).or_default();
                let batch = drain_batch(&mut queue.pending);
                if !batch.is_empty() {
                    return batch;
                }
                if wait_secs == 0 || tokio::time::Instant::now() >= deadline {
                    if queue.waiters.is_empty() && Arc::strong_count(&queue.notify) == 1 {
                        map.remove(device);
                    }
                    return Vec::new();
                }
                Arc::clone(&queue.notify)
            };

            // Register for wakeup before re-checking `pending` so no enqueue can be missed.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut map = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(queue) = map.get_mut(device) {
                    let batch = drain_batch(&mut queue.pending);
                    if !batch.is_empty() {
                        return batch;
                    }
                }
            }

            let sleep = tokio::time::sleep_until(deadline);
            tokio::pin!(sleep);
            tokio::select! {
                () = &mut notified => {}
                () = &mut sleep => return Vec::new(),
                () = &mut stopped => return Vec::new(),
            }
        }
    }

    /// Delivers a device's reply to an in-flight RPC call.
    pub(crate) fn reply(&self, device: &DeviceName, id: &str, reply: DeviceRpcReply) -> Result<()> {
        let waiter = {
            let mut map = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(queue) = map.get_mut(device) else {
                return Err(not_found(
                    format!("no pending rpc call {id} for device {device}"),
                    "the call may have timed out or already been answered",
                ));
            };
            queue.pending.retain(|c| c.id != id);
            let tx = queue.waiters.remove(id);
            if queue.pending.is_empty()
                && queue.waiters.is_empty()
                && Arc::strong_count(&queue.notify) == 1
            {
                map.remove(device);
            }
            tx
        };
        let Some(tx) = waiter else {
            return Err(not_found(
                format!("no pending rpc call {id} for device {device}"),
                "the call may have timed out or already been answered",
            ));
        };
        let _ = tx.send(reply);
        Ok(())
    }
}

fn drain_batch(pending: &mut VecDeque<DeviceRpcCall>) -> Vec<DeviceRpcCall> {
    let take = pending.len().min(MAX_POLL_BATCH);
    pending.drain(..take).collect()
}
