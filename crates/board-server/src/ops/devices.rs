use board_proto::{
    AdminUpdateDevice, DeviceCapacity, DeviceEntry, DeviceHeartbeat, DeviceName, DeviceRole,
    DeviceStatus, DeviceToken, EventType, Taint, TaintEffect,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::auth::{hash_token, new_token};
use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ts};
use crate::error::{Result, invalid, not_found};
use crate::text::normalize_repo;

pub use board_proto::DeviceInfo;

/// Most taints one device may carry.
const DEVICE_TAINTS_MAX: usize = 20;
/// Most capabilities one device heartbeat may advertise.
const DEVICE_CAPABILITIES_MAX: usize = 64;
/// Most runners one device heartbeat may advertise.
const DEVICE_RUNNERS_MAX: usize = 32;
/// Most repos one device heartbeat may advertise.
const DEVICE_REPOS_MAX: usize = 200;
/// Most protocol features one device heartbeat may advertise.
const DEVICE_FEATURES_MAX: usize = 32;
/// Bounds on `DeviceCapacity::weight_units` and `free_units`.
const DEVICE_UNITS_MAX: u32 = 10_000;
/// Bounds on `DeviceEntry::preference`.
const DEVICE_PREFERENCE_RANGE: std::ops::RangeInclusive<i32> = -1000..=1000;

const DEVICE_ENTRY_COLUMNS: &str = "name, role, drain, os, arch, capacity_json, free_units, \
     capabilities_json, runners_json, repos_json, taints_json, preference, \
     protocol_features_json, heartbeat_at";

fn device_status(heartbeat_at: Option<i64>, now_ms: i64, stale_ms: i64) -> DeviceStatus {
    match heartbeat_at {
        None => DeviceStatus::Offline,
        Some(at) if now_ms - at <= stale_ms => DeviceStatus::Online,
        Some(_) => DeviceStatus::Stale,
    }
}

fn device_entry_row(r: &Row<'_>, now_ms: i64, stale_ms: i64) -> rusqlite::Result<DeviceEntry> {
    let heartbeat_at: Option<i64> = r.get(13)?;
    Ok(DeviceEntry {
        name: parse_id(&r.get::<_, String>(0)?)?,
        role: parse_enum(&r.get::<_, String>(1)?)?,
        status: device_status(heartbeat_at, now_ms, stale_ms),
        drain: r.get(2)?,
        os: r.get(3)?,
        arch: r.get(4)?,
        capacity: r
            .get::<_, Option<String>>(5)?
            .map(|s| parse_json::<DeviceCapacity>(&s))
            .transpose()?,
        free_units: r.get(6)?,
        capabilities: parse_json(&r.get::<_, String>(7)?)?,
        runners: parse_json(&r.get::<_, String>(8)?)?,
        repos: parse_json(&r.get::<_, String>(9)?)?,
        taints: parse_json(&r.get::<_, String>(10)?)?,
        preference: r.get(11)?,
        protocol_features: parse_json(&r.get::<_, String>(12)?)?,
        heartbeat_at: heartbeat_at.map(ts),
    })
}

pub(crate) fn get_active_entry(cx: &Ctx<'_>, name: &DeviceName) -> Result<DeviceEntry> {
    let now_ms = cx.now_ms();
    let stale_ms = cx.s.device_stale_ms();
    cx.db
        .prepare_cached(&format!(
            "SELECT {DEVICE_ENTRY_COLUMNS} FROM devices WHERE name = ?1 AND revoked_at IS NULL"
        ))?
        .query_row([name.as_str()], |r| device_entry_row(r, now_ms, stale_ms))
        .optional()?
        .ok_or_else(|| {
            not_found(
                format!("no active device named {name}"),
                "GET /v1/devices lists the active devices",
            )
        })
}

/// Checks a compact single-token label (no whitespace or control chars, 1..=`max` chars).
pub(crate) fn check_token(field: &str, value: &str, max: usize) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid(
            format!("{field} is empty"),
            format!("send a non-empty {field} token"),
        ));
    }
    if trimmed.chars().count() > max {
        return Err(invalid(
            format!("{field} is too long (limit {max} characters)"),
            format!("shorten {field} to at most {max} characters"),
        ));
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid(
            format!("{field} {trimmed:?} contains whitespace or control characters"),
            format!("send {field} as a plain token without spaces or control characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

pub(crate) fn check_token_list(
    field: &str,
    items: Vec<String>,
    max_items: usize,
    max_chars: usize,
) -> Result<Vec<String>> {
    if items.len() > max_items {
        return Err(invalid(
            format!(
                "{field} has {} entries; the limit is {max_items}",
                items.len()
            ),
            format!("send at most {max_items} {field} entries"),
        ));
    }
    let mut out = Vec::new();
    for item in items {
        let clean = check_token(field, &item, max_chars)?;
        if !out.contains(&clean) {
            out.push(clean);
        }
    }
    Ok(out)
}

/// Validates an admin device governance update (`PATCH /v1/admin/devices/{name}`).
pub(crate) fn validate_admin_update(mut req: AdminUpdateDevice) -> Result<AdminUpdateDevice> {
    if req.role == Some(DeviceRole::Unknown) {
        return Err(invalid(
            "unknown device role",
            "send role as control, runner or service_host",
        ));
    }
    if let Some(pref) = req.preference
        && !DEVICE_PREFERENCE_RANGE.contains(&pref)
    {
        return Err(invalid(
            format!("preference={pref} is out of range"),
            format!(
                "send preference between {} and {}",
                DEVICE_PREFERENCE_RANGE.start(),
                DEVICE_PREFERENCE_RANGE.end()
            ),
        ));
    }
    if let Some(taints) = req.taints.take() {
        if taints.len() > DEVICE_TAINTS_MAX {
            return Err(invalid(
                format!("a device may carry at most {DEVICE_TAINTS_MAX} taints"),
                format!("send at most {DEVICE_TAINTS_MAX} taints"),
            ));
        }
        let mut clean = Vec::new();
        for taint in taints {
            if taint.effect == TaintEffect::Unknown {
                return Err(invalid(
                    "unknown taint effect",
                    "send effect as no_schedule or prefer_no_schedule",
                ));
            }
            let key = check_token("taint key", &taint.key, 64)?;
            let value = taint
                .value
                .map(|v| check_token("taint value", &v, 64))
                .transpose()?;
            let entry = Taint {
                key,
                value,
                effect: taint.effect,
            };
            if !clean.contains(&entry) {
                clean.push(entry);
            }
        }
        req.taints = Some(clean);
    }
    Ok(req)
}

/// Validates a device heartbeat (`PUT /v1/devices/{name}/heartbeat`).
pub(crate) fn validate_heartbeat(mut req: DeviceHeartbeat) -> Result<DeviceHeartbeat> {
    req.os = req.os.map(|v| check_token("os", &v, 40)).transpose()?;
    req.arch = req.arch.map(|v| check_token("arch", &v, 40)).transpose()?;
    if let Some(cap) = req.capacity {
        if cap.weight_units > DEVICE_UNITS_MAX {
            return Err(invalid(
                format!("capacity.weight_units={} is out of range", cap.weight_units),
                format!("send capacity.weight_units between 0 and {DEVICE_UNITS_MAX}"),
            ));
        }
        if cap.mem_gib.is_some_and(|m| m > 1_000_000) {
            return Err(invalid(
                "capacity.mem_gib is out of range",
                "send capacity.mem_gib as a realistic GiB count",
            ));
        }
        if cap.cpu_cores.is_some_and(|c| c > DEVICE_UNITS_MAX) {
            return Err(invalid(
                "capacity.cpu_cores is out of range",
                format!("send capacity.cpu_cores between 0 and {DEVICE_UNITS_MAX}"),
            ));
        }
        if let Some(free) = req.free_units
            && free > cap.weight_units
        {
            return Err(invalid(
                format!(
                    "free_units ({free}) exceeds capacity.weight_units ({})",
                    cap.weight_units
                ),
                "send free_units <= capacity.weight_units",
            ));
        }
    } else if req.free_units.is_some_and(|f| f > DEVICE_UNITS_MAX) {
        return Err(invalid(
            "free_units is out of range",
            format!("send free_units between 0 and {DEVICE_UNITS_MAX}"),
        ));
    }
    req.capabilities = check_token_list(
        "capabilities",
        req.capabilities,
        DEVICE_CAPABILITIES_MAX,
        64,
    )?;
    req.runners = check_token_list("runners", req.runners, DEVICE_RUNNERS_MAX, 40)?;
    if req.repos.len() > DEVICE_REPOS_MAX {
        return Err(invalid(
            format!("a device may advertise at most {DEVICE_REPOS_MAX} repos"),
            format!("send at most {DEVICE_REPOS_MAX} repos"),
        ));
    }
    let mut repos = Vec::new();
    for raw in req.repos {
        let key = normalize_repo(&raw)?;
        if !repos.contains(&key) {
            repos.push(key);
        }
    }
    req.repos = repos;
    req.protocol_features = check_token_list(
        "protocol_features",
        req.protocol_features,
        DEVICE_FEATURES_MAX,
        64,
    )?;
    Ok(req)
}

/// Creates a device and returns its token, shown once. A revoked name may be reused.
pub(crate) fn create(cx: &mut Ctx<'_>, name: &DeviceName, user: bool) -> Result<DeviceToken> {
    let revoked: Option<Option<i64>> = cx
        .db
        .query_row(
            "SELECT revoked_at FROM devices WHERE name = ?1",
            [name.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(None) = revoked {
        return Err(invalid(
            format!("device {name} already exists"),
            format!("revoke it first (DELETE /v1/admin/devices/{name}) or pick another name"),
        ));
    }
    let token = new_token();
    cx.db.execute(
        "INSERT INTO devices (name, token_hash, user, created_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (name) DO UPDATE SET token_hash = ?2, user = ?3, created_at = ?4, \
         revoked_at = NULL, role = 'runner', os = NULL, arch = NULL, capacity_json = NULL, \
         free_units = NULL, capabilities_json = '[]', runners_json = '[]', repos_json = '[]', \
         taints_json = '[]', preference = 0, protocol_features_json = '[]', drain = 0, \
         heartbeat_at = NULL",
        params![name.as_str(), hash_token(&token), user, cx.now_ms()],
    )?;
    Ok(DeviceToken {
        name: name.clone(),
        token,
    })
}

pub(crate) fn list(cx: &Ctx<'_>) -> Result<Vec<DeviceInfo>> {
    Ok(cx
        .db
        .prepare_cached("SELECT name, user, created_at, revoked_at FROM devices ORDER BY name")?
        .query_map([], |r| {
            Ok(DeviceInfo {
                name: parse_id(&r.get::<_, String>(0)?)?,
                user: r.get(1)?,
                created_at: ts(r.get(2)?),
                revoked_at: r.get::<_, Option<i64>>(3)?.map(ts),
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Lists all non-revoked devices with live status computed from `heartbeat_at` against the
/// injectable clock (`GET /v1/devices`).
pub(crate) fn list_entries(cx: &Ctx<'_>) -> Result<Vec<DeviceEntry>> {
    let now_ms = cx.now_ms();
    let stale_ms = cx.s.device_stale_ms();
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {DEVICE_ENTRY_COLUMNS} FROM devices WHERE revoked_at IS NULL ORDER BY name"
        ))?
        .query_map([], |r| device_entry_row(r, now_ms, stale_ms))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Updates only live heartbeat fields for `name` (`PUT /v1/devices/{name}/heartbeat`).
/// Never touches admin governance fields (`role`, `taints`, `preference`, `drain`).
pub(crate) fn heartbeat(
    cx: &mut Ctx<'_>,
    name: &DeviceName,
    req: DeviceHeartbeat,
) -> Result<DeviceEntry> {
    let cap_json = req
        .capacity
        .map(|c| serde_json::to_string(&c).expect("capacity serializes"));
    let caps_json = serde_json::to_string(&req.capabilities).expect("strings serialize");
    let runners_json = serde_json::to_string(&req.runners).expect("strings serialize");
    let repos_json = serde_json::to_string(&req.repos).expect("strings serialize");
    let features_json = serde_json::to_string(&req.protocol_features).expect("strings serialize");
    let updated = cx.db.execute(
        "UPDATE devices SET os = ?2, arch = ?3, capacity_json = ?4, free_units = ?5, \
         capabilities_json = ?6, runners_json = ?7, repos_json = ?8, \
         protocol_features_json = ?9, heartbeat_at = ?10 \
         WHERE name = ?1 AND revoked_at IS NULL",
        params![
            name.as_str(),
            req.os,
            req.arch,
            cap_json,
            req.free_units,
            caps_json,
            runners_json,
            repos_json,
            features_json,
            cx.now_ms(),
        ],
    )?;
    if updated == 0 {
        return Err(not_found(
            format!("no active device named {name}"),
            "GET /v1/devices lists the active devices",
        ));
    }
    get_active_entry(cx, name)
}

/// User-only update of device plane metadata (`PATCH /v1/admin/devices/{name}`).
/// Emits a durable `device_updated` event recording the actor, changed fields, and values.
pub(crate) fn admin_update(
    cx: &mut Ctx<'_>,
    actor: &str,
    name: &DeviceName,
    req: AdminUpdateDevice,
) -> Result<DeviceEntry> {
    let before = get_active_entry(cx, name)?;
    let write_role = req.role.is_some();
    let write_taints = req.taints.is_some();
    let role = req.role.unwrap_or(before.role);
    let taints = req.taints.unwrap_or_else(|| before.taints.clone());
    let preference = req.preference.unwrap_or(before.preference);
    let drain = req.drain.unwrap_or(before.drain);

    let mut fields: Vec<&str> = Vec::new();
    if write_role && role != before.role {
        fields.push("role");
    }
    if write_taints && taints != before.taints {
        fields.push("taints");
    }
    if preference != before.preference {
        fields.push("preference");
    }
    if drain != before.drain {
        fields.push("drain");
    }
    if fields.is_empty() {
        return Ok(before);
    }

    let role_changed = fields.contains(&"role");
    let taints_changed = fields.contains(&"taints");
    let taints_json = serde_json::to_string(&taints).expect("taints serialize");
    cx.db.execute(
        "UPDATE devices SET \
         role = CASE WHEN ?6 THEN ?2 ELSE role END, \
         taints_json = CASE WHEN ?7 THEN ?3 ELSE taints_json END, \
         preference = ?4, drain = ?5 \
         WHERE name = ?1 AND revoked_at IS NULL",
        params![
            name.as_str(),
            enum_str(&role),
            taints_json,
            preference,
            drain,
            role_changed,
            taints_changed,
        ],
    )?;
    let mut payload = serde_json::json!({
        "device": name,
        "fields": fields,
    });
    if role_changed {
        payload["role"] = serde_json::json!(role);
    }
    if taints_changed {
        payload["taints"] = serde_json::json!(taints);
    }
    if fields.contains(&"preference") {
        payload["preference"] = serde_json::json!(preference);
    }
    if fields.contains(&"drain") {
        payload["drain"] = serde_json::json!(drain);
    }
    cx.emit(None, actor, EventType::DeviceUpdated, None, payload)?;
    get_active_entry(cx, name)
}

pub(crate) fn revoke(cx: &mut Ctx<'_>, name: &DeviceName) -> Result<()> {
    let updated = cx.db.execute(
        "UPDATE devices SET revoked_at = ?2 WHERE name = ?1 AND revoked_at IS NULL",
        params![name.as_str(), cx.now_ms()],
    )?;
    if updated == 0 {
        return Err(not_found(
            format!("no active device named {name}"),
            "GET /v1/admin/devices lists the devices",
        ));
    }
    Ok(())
}
